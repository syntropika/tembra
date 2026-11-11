import type { Description } from "./types.ts";
import type { Declaration } from "../authoring/declaration.ts";
import type {
  OrbDefinition,
  ServiceDefinition,
  HookDefinition,
  Contribution,
} from "../authoring/types.ts";
import { series, waterfall, bail } from "../authoring/hooks.ts";
import { KernelOp } from "../bridge/protocol.ts";
import {
  declareFactory,
  bindCaptured,
  signature,
  signatures,
  validateEnvironment,
  admitFactory,
  disposeOrb,
  isolatedRevision,
  releaseEnvironment,
  type AdaptedOrb,
  type AdaptedService,
} from "#tembra-adapters";
import { Context } from "../runtime/context.ts";
import type { Environment } from "../runtime/records.ts";

import { check, encode, packet, checkedResult, VALUE_LIMIT, type Encoded } from "./codec.ts";
import { HTTP_HANDLER, IngressExchange } from "./http-ingress.ts";
import { Gate } from "./gate.ts";
import { Scope } from "./scope.ts";
import {
  query,
  type ContractPort,
  type HookBinding,
  type CheckedImplementation,
  type InstallationPlan,
  type InterpreterPort,
  type ExecutionScope,
} from "./types.ts";
import { lift, shapes, type Signature, type WitFunction, type WitTypes } from "./wit-shape.ts";

/** The contract's `describe` answer, as far as this adapter reads it. */
type Described = WitTypes & {
  imports: { binding: string; functions?: Record<string, WitFunction> }[];
  exports: {
    binding: string;
    contract?: string;
    functions?: Record<string, WitFunction>;
  }[];
};

/** One isolated revision translates callbacks into ordinary SDK declarations and calls. */
export class Revision {
  readonly gate = new Gate();
  readonly imports = new Map<string, string>();
  readonly importSignatures = new Map<string, string>();
  readonly importMethods = new Map<string, Map<string, { name: string; slot: number }>>();
  readonly hooks = new Map<string, HookBinding>();
  /** Where WIT `u64`/`s64` occur in each bound slot's arguments and result. */
  readonly signatures = new Map<number, Signature>();
  readonly scopes = new Map<number, Scope>();
  active = false;
  #closed = false;

  /** Both transports share the resolved Rust contract; the SDK remains the sole lifecycle adapter. */
  constructor(
    readonly port: InterpreterPort,
    readonly contract: ContractPort,
    readonly timeout: number,
  ) {}

  /** Run pure synchronous guest work only while its enclosing factory owns admission. */
  private async declare(command: string, scope: ExecutionScope): Promise<unknown> {
    this.active = true;
    try {
      return await this.port.declare(command, scope);
    } finally {
      scope.cancel();
      this.active = false;
    }
  }

  /** A registered resource `open` callback of the interpreter guest. */
  resourceOpen(environment: number, callback: number, scope: ExecutionScope): Promise<number> {
    return this.execute(
      JSON.stringify({ kind: "callback", environment, callback, value: null }),
      scope,
    ) as Promise<number>;
  }

  /** A registered resource `close` callback of the interpreter guest. */
  resourceClose(
    environment: number,
    callback: number,
    value: number,
    scope: ExecutionScope,
  ): Promise<unknown> {
    return this.execute(JSON.stringify({ kind: "callback", environment, callback, value }), scope);
  }

  /**
   * Run same-revision resource callbacks and decode their reply after physical settlement.
   * Cross-revision methods use executeText and check the original bytes before decoding.
   */
  async execute(command: string, scope: ExecutionScope): Promise<unknown> {
    return JSON.parse(await this.executeText(command, scope));
  }

  /** Retain the original reply until its prepared result signature has checked the bytes. */
  private async executeText(command: string, scope: ExecutionScope): Promise<string> {
    const release = await this.gate.enter(undefined, scope.signal);
    this.active = true;
    try {
      scope.check();
      return await this.port.execute(command, scope);
    } finally {
      scope.cancel();
      this.active = false;
      release();
    }
  }

  /**
   * The host-local incoming HTTP service. It has no contract signature, so no WIT dependency can
   * bind it, and its method accepts only an exchange issued by `Host.handleHttp`. One request is
   * one callback of this revision: it holds the gate until the response delivery ended.
   */
  private ingress(
    contract: string,
    description: { javascriptNames: Record<string, string> },
    create: (service: string) => (declaration: Declaration) => Promise<Scope>,
  ): ServiceDefinition {
    if (contract !== HTTP_HANDLER) throw new Error(`Unsupported incoming HTTP export ${contract}`);
    const service = description.javascriptNames["handler"];
    const method = description.javascriptNames["handle"];
    if (!service || !method) throw new Error("Missing prepared incoming HTTP handler names");
    const declared = (JSON.parse(this.port.description()) as Description).services[service];
    if (!declared?.methods.includes(method))
      throw new Error(`The Orb exports ${HTTP_HANDLER} without a ${service}.${method} handler`);
    return {
      [declareFactory]: create(service),
      lifetime: declared.lifetime,
      linger: declared.linger,
      [admitFactory]: () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2))),
      methods: {
        handle: async (environment: Scope, context: Context, value: unknown): Promise<void> => {
          const exchange = IngressExchange.claim(value);
          if (!this.port.ingress)
            throw new Error("This host does not serve incoming HTTP for source Orbs yet");
          const entered = new Context(
            context.session,
            context.root,
            context.invocation,
            context.session.kernel.command(KernelOp.EnterExecution, {
              call: context.invocation!.id,
              context: context.frame || undefined,
            }),
          );
          const scope = environment.exchange({ context: entered });
          const release = await this.gate.enter(undefined, scope.signal);
          this.active = true;
          try {
            scope.check();
            await this.port.ingress(
              { environment: environment.id, service, method },
              exchange,
              scope,
            );
          } finally {
            scope.cancel();
            this.active = false;
            release();
          }
        },
      },
    } as AdaptedService;
  }

  /** Build callback definitions from WIT rather than asking consumers to repeat service metadata. */
  definition(
    identity: Pick<OrbDefinition, "name" | "apiVersion">,
    installation: InstallationPlan,
  ): OrbDefinition {
    const { importServices: imports } = installation;
    const checked = query<CheckedImplementation>(this.contract, {
      kind: "implementation",
      name: identity.name,
      apiVersion: identity.apiVersion,
      value: JSON.parse(this.port.description()) as Description,
      bindings: installation.bindings,
    });
    const { plan } = checked;
    const description = query<Described>(this.contract, { kind: "describe" });
    const compile = shapes(description);
    const names = description.javascriptNames;
    /**
     * The callback of the exported hook interface `contract`. Hook and contribution bindings carry
     * that full interface identity (the contract's own resolver chose it), so it is looked up as is.
     */
    const callback = (contract: string, method: string): WitFunction => {
      const exported = description.exports.find((item) => item.contract === contract);
      const found = Object.entries(exported?.functions ?? {}).find(
        ([name]) => names[name] === method,
      );
      if (!found) throw new Error(`Missing prepared signature for ${contract}#${method}`);
      return found[1];
    };
    for (const hook of checked.hooks.hooks) {
      this.hooks.set(hook.alias, hook);
      this.signatures.set(hook.slot, compile(callback(hook.name, hook.method)));
    }
    for (const [binding, service] of Object.entries(imports)) {
      if (checked.hooks.contributions.some((contribution) => contribution.binding === binding))
        continue;
      this.imports.set(binding, service);
      const provider = installation.bindings[binding];
      const requirement =
        provider?.kind === "orb"
          ? plan.orb.requires.find((item) => item.orb === provider.name)
          : undefined;
      const signature =
        requirement && Object.hasOwn(requirement.signatures, service)
          ? requirement.signatures[service]
          : undefined;
      if (signature) this.importSignatures.set(binding, signature);
      const selected = description.imports.find((item) => item.binding === binding);
      if (!selected?.functions) throw new Error("Missing prepared import signatures");
      const methods = new Map<string, { name: string; slot: number }>();
      for (const [name, fn] of Object.entries(selected.functions)) {
        const spelling = description.javascriptNames[name];
        if (!spelling || methods.has(spelling)) throw new Error("Ambiguous prepared import method");
        const slot = this.contract.bind(binding, name, false);
        this.signatures.set(slot, compile(fn));
        methods.set(spelling, { name, slot });
      }
      this.importMethods.set(binding, methods);
    }
    let rootScope: Scope | undefined;
    const create = (service: string | null) => async (declaration: Declaration) => {
      const scope = new Scope(this, declaration, service);
      if (service === null) rootScope = scope;
      this.scopes.set(scope.id, scope);
      await this.declare(
        JSON.stringify({ kind: "create", environment: scope.id, service }),
        scope.exchange({ declaration }),
      );
      return scope;
    };
    const services: Record<string, ServiceDefinition> = {};
    for (const service of checked.services) {
      const { interface: binding, alias } = service;
      const exported = description.exports.find((item) => item.binding === binding)?.functions;
      services[service.name] = {
        [signature]: service.signature,
        [declareFactory]: create(alias),
        lifetime: service.lifetime,
        linger: service.linger,
        [admitFactory]: () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2))),
        methods: Object.fromEntries(
          Object.entries(service.methods).map(([method, spelling]) => {
            const slot = this.contract.bind(binding, method, true);
            const fn = exported?.[method];
            if (!fn) throw new Error(`Missing prepared signature for ${binding}#${method}`);
            const shape = compile(fn);
            this.signatures.set(slot, shape);
            const invoke = async (
              environment: Scope,
              context: Context,
              input: Encoded,
            ): Promise<Encoded> => {
              const entered = new Context(
                context.session,
                context.root,
                context.invocation,
                context.session.kernel.command(KernelOp.EnterExecution, {
                  call: context.invocation!.id,
                  context: context.frame || undefined,
                }),
              );
              const text = await this.executeText(
                packet(
                  {
                    kind: "invoke",
                    environment: environment.id,
                    service: alias,
                    method: spelling,
                  },
                  "args",
                  input,
                ),
                environment.exchange({ context: entered }),
              );
              return checkedResult(this.contract, slot, text);
            };
            return [
              method,
              bindCaptured(
                async (environment: Scope, context: Context, ...args: unknown[]) => {
                  // Host values are captured once, with WIT 64-bit positions typed in the walk.
                  const input = check(
                    this.contract,
                    slot,
                    true,
                    encode(args, VALUE_LIMIT, shape.params),
                  );
                  const text = (await invoke(environment, context, input)).text;
                  return lift(shape.result, JSON.parse(text));
                },
                service.signature,
                invoke,
              ),
            ];
          }),
        ),
      } as AdaptedService;
    }
    // Service names are kernel-global, `<orb>::<interface>` like the contract's own services.
    if (plan.ingress)
      services[`${identity.name}::${HTTP_HANDLER}`] = this.ingress(
        plan.ingress,
        description,
        create,
      );
    const hooks: Record<string, HookDefinition> = {};
    for (const binding of checked.hooks.hooks) {
      const hook =
        binding.composition === "series"
          ? series()
          : binding.composition === "bail"
            ? bail()
            : waterfall();
      hooks[binding.name] = Object.assign(hook, {
        [signature]: binding.signature,
      });
    }
    const contributes: Record<string, Record<string, Contribution>> = {};
    for (const binding of checked.hooks.contributions) {
      const shape = compile(callback(binding.name, binding.method));
      this.signatures.set(binding.slot, shape);
      (contributes[binding.target] ??= {})[binding.name] = {
        priority: binding.priority,
        handle: async (context, input) => {
          if (!rootScope || this.scopes.get(rootScope.id) !== rootScope)
            throw new Error("Contribution Orb scope is unavailable");
          const entered = new Context(
            context.session,
            context.root,
            context.invocation,
            context.session.kernel.command(KernelOp.EnterExecution, {
              call: context.invocation!.id,
              context: context.frame || undefined,
            }),
          );
          const arguments_ = check(
            this.contract,
            binding.slot,
            true,
            encode([input], VALUE_LIMIT, shape.params),
          );
          const result = await this.executeText(
            packet(
              {
                kind: "contribute",
                environment: rootScope.id,
                binding: binding.binding,
                service: binding.alias,
                method: binding.method,
              },
              "args",
              arguments_,
            ),
            rootScope.exchange({ context: entered }),
          );
          const value = lift(
            shape.result,
            JSON.parse(checkedResult(this.contract, binding.slot, result).text),
          );
          return binding.optional && value === null ? undefined : value;
        },
      };
    }
    return {
      name: identity.name,
      apiVersion: identity.apiVersion,
      services,
      hooks,
      contributes,
      requires: Object.fromEntries(
        plan.orb.requires.map((requirement) => [
          requirement.orb,
          { ...requirement, [signatures]: requirement.signatures },
        ]),
      ),
      [isolatedRevision]: true,
      [declareFactory]: create(null),
      [validateEnvironment]: async (environment: Environment, value: unknown) => {
        const scope = this.scopes.get(environment.id);
        if (!scope) throw new Error("Validator scope is gone");
        const release = await this.gate.enter();
        try {
          for (const callback of scope.validators)
            await this.declare(
              packet(
                { kind: "validate", environment: environment.id, callback },
                "value",
                encode(value),
              ),
              scope.exchange({ validation: true }),
            );
        } finally {
          release();
        }
      },
      [admitFactory]: () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2))),
      [releaseEnvironment]: async (environment: Environment) => {
        const scope = this.scopes.get(environment.id);
        if (!scope) return;
        this.scopes.delete(scope.id);
        const release = await this.gate.enter();
        try {
          await this.port.release(scope.id);
        } finally {
          release();
        }
      },
      [disposeOrb]: async () => {
        if (this.#closed) return;
        if (this.scopes.size) throw new Error("Interpreter still owns physical scopes");
        await this.port.close();
        this.#closed = true;
      },
    } as AdaptedOrb;
  }
}
