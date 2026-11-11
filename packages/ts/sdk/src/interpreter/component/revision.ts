/**
 * One installed component revision, adapted to ordinary SDK Orb definitions.
 *
 * The checked contract plans services, hooks and contributions exactly as for native hosts. A
 * source component describes its implementation through `tembra:orb/lifecycle.describe`, which is
 * checked before any factory runs; a compiled component is described from its contract alone and
 * selects its Orb imports through default `main` handles. Every guest entry is one callback
 * admitted by the revision gate and served by the component port.
 */
import type { Declaration } from "../../authoring/declaration.ts";
import type {
  Contribution,
  HookDefinition,
  OrbDefinition,
  ServiceDefinition,
} from "../../authoring/types.ts";
import { bail, series, waterfall } from "../../authoring/hooks.ts";
import { KernelOp } from "../../bridge/protocol.ts";
import {
  admitFactory,
  bindCaptured,
  declareFactory,
  disposeOrb,
  isolatedRevision,
  releaseEnvironment,
  signature,
  signatures,
  validateEnvironment,
  type AdaptedOrb,
  type AdaptedService,
} from "#tembra-adapters";
import { Context } from "../../runtime/context.ts";
import type { Environment } from "../../runtime/records.ts";
import { check, checkedResult, encode, VALUE_LIMIT, type Encoded } from "../codec.ts";
import { Gate } from "../gate.ts";
import { Scope, type ScopeOwner } from "../scope.ts";
import {
  query,
  type CheckedImplementation,
  type ContractPort,
  type Description,
  type ExecutionScope,
  type HookBinding,
  type InstallationPlan,
} from "../types.ts";
import { lift, shapes, type Signature, type WitFunction, type WitTypes } from "../wit-shape.ts";
import type { ComponentPort } from "./port.ts";
import { HTTP_HANDLER, IngressExchange, type IncomingHttpRequest } from "../http-ingress.ts";

/** The HTTP adapter's per-instance incoming bridge (implemented in `wasi-http.ts`). */
export interface IngressBridge {
  request(incoming: IncomingHttpRequest): unknown;
  respond(response: unknown, exchange: IngressExchange): Promise<void>;
}
import { converters, type FunctionValues, type GraphFunction, type TypeGraph } from "./values.ts";

export const LIFECYCLE = "tembra:orb/lifecycle@0.1.0";

/** The contract's `describe` answer, as far as component execution reads it. */
export type Described = WitTypes &
  TypeGraph & {
    imports: {
      binding: string;
      contract?: string;
      type_only?: boolean;
      functions?: Record<string, WitFunction>;
    }[];
    exports: {
      binding: string;
      contract?: string;
      functions?: Record<string, WitFunction>;
    }[];
  };

type Lifecycle = Record<string, (...args: unknown[]) => unknown>;

/** The guest's JavaScript spelling of a WIT name (`heck` lower camel case, as in Rust). */
export function javascriptName(name: string): string {
  return name
    .split("-")
    .map((part, index) => (index === 0 ? part : part.charAt(0).toUpperCase() + part.slice(1)))
    .join("");
}

/** A guest entry that refuses every host request: `describe` and `release` need none. */
const NO_AUTHORITY: ExecutionScope = {
  check() {},
  cancel() {},
  sync() {
    throw new Error("This callback has no host scope");
  },
  async() {
    return Promise.reject(new Error("This callback has no host scope"));
  },
};

/** Check a value from the guest's described implementation record. */
function described(value: unknown): Description {
  const record = value as {
    name: string;
    apiVersion: number;
    services: {
      alias: string;
      methods: string[];
      lifetime?: string;
      linger?: number;
      access: boolean;
    }[];
    hooks: { alias: string; composition: string }[];
    contributes: { binding: string; priority: number }[];
  };
  const services: Description["services"] = {};
  for (const service of record.services) {
    if (Object.hasOwn(services, service.alias)) throw new Error("duplicate described service");
    services[service.alias] = {
      methods: [...service.methods],
      ...(service.lifetime === undefined ? {} : { lifetime: service.lifetime as never }),
      ...(service.linger === undefined ? {} : { linger: service.linger }),
      access: service.access,
    };
  }
  const hooks: NonNullable<Description["hooks"]> = {};
  for (const hook of record.hooks) {
    if (Object.hasOwn(hooks, hook.alias)) throw new Error("duplicate described hook");
    hooks[hook.alias] = hook.composition as never;
  }
  const contributes: NonNullable<Description["contributes"]> = {};
  for (const contribution of record.contributes) {
    if (Object.hasOwn(contributes, contribution.binding))
      throw new Error("duplicate described contribution");
    contributes[contribution.binding] = contribution.priority;
  }
  return {
    name: record.name,
    apiVersion: record.apiVersion,
    services,
    hooks,
    contributes,
  };
}

/** A compiled component's description: one service per callable export interface. */
export function compiledDescription(
  graph: Described,
  identity: { name: string; apiVersion: number },
  contributions: Readonly<Record<string, number>>,
): Description {
  const services: Description["services"] = {};
  for (const entry of graph.exports) {
    if (!entry.contract || !entry.functions || !Object.keys(entry.functions).length) continue;
    if (Object.hasOwn(contributions, entry.contract)) continue;
    const name = entry.contract.slice(entry.contract.indexOf("/") + 1).split("@")[0]!;
    services[javascriptName(name)] = {
      methods: Object.keys(entry.functions).map(javascriptName),
    };
  }
  return { ...identity, services, contributes: { ...contributions } };
}

export class ComponentRevision implements ScopeOwner {
  readonly gate = new Gate();
  readonly imports = new Map<string, string>();
  readonly importSignatures = new Map<string, string>();
  readonly importMethods = new Map<string, Map<string, { name: string; slot: number }>>();
  readonly hooks = new Map<string, HookBinding>();
  readonly signatures = new Map<number, Signature>();
  readonly scopes = new Map<number, Scope>();
  #closed = false;

  constructor(
    readonly port: ComponentPort,
    readonly contract: ContractPort,
    readonly graph: Described,
    readonly timeout: number,
    readonly source: boolean,
  ) {}

  /** The checked hook an Orb runs through its self-import of `contract`. */
  hookFor(contract: string): HookBinding | undefined {
    for (const hook of this.hooks.values()) if (hook.name === contract) return hook;
    return undefined;
  }

  /** The JCO lifecycle export of a source component. */
  private lifecycle(): Lifecycle {
    const exported = this.port.exports?.[LIFECYCLE];
    if (!exported) throw new Error("The component exports no Tembra lifecycle");
    return exported as Lifecycle;
  }

  /** Run one lifecycle callback; its WIT `result` error is a recoverable refusal. */
  private async callback<T>(
    environment: number,
    scope: ExecutionScope,
    call: (lifecycle: Lifecycle) => unknown,
  ): Promise<T> {
    const outcome = await this.port.run(BigInt(environment), scope, async () => {
      try {
        return { ok: (await call(this.lifecycle())) as T };
      } catch (error) {
        if (error === null || typeof error !== "object" || !Object.hasOwn(error, "payload"))
          throw error;
        return { error: String((error as { payload: unknown }).payload) };
      }
    });
    if ("error" in outcome) throw new Error(outcome.error);
    return outcome.ok;
  }

  /** Read and check the guest's own description once, before any factory. */
  async describe(
    identity: { name: string; apiVersion: number },
    contributions: Record<string, number>,
  ) {
    if (!this.source)
      return compiledDescription(
        this.graph,
        { name: identity.name, apiVersion: identity.apiVersion },
        contributions,
      );
    return described(await this.port.run(0n, NO_AUTHORITY, () => this.lifecycle().describe!()));
  }

  async resourceOpen(
    environment: number,
    callback: number,
    scope: ExecutionScope,
  ): Promise<number> {
    const release = await this.gate.enter(undefined, scope.signal);
    try {
      return await this.callback<number>(environment, scope, (lifecycle) =>
        lifecycle.open!(callback),
      );
    } finally {
      scope.cancel();
      release();
    }
  }

  async resourceClose(
    environment: number,
    callback: number,
    value: number,
    scope: ExecutionScope,
  ): Promise<unknown> {
    const release = await this.gate.enter(undefined, scope.signal);
    try {
      // A quarantined instance has no guest left to close; its providers are still disposed
      // after their physical join when the revision retires.
      if (!this.port.usable) return undefined;
      return await this.callback(environment, scope, (lifecycle) =>
        lifecycle.close!(callback, value),
      );
    } finally {
      scope.cancel();
      release();
    }
  }

  /** Call one typed export under an admitted callback and answer checked result bytes. */
  private async invoke(
    key: string,
    spelling: string,
    values: FunctionValues,
    slot: number,
    shape: Signature,
    environment: Scope,
    scope: ExecutionScope,
    input: Encoded,
  ): Promise<Encoded> {
    const release = await this.gate.enter(undefined, scope.signal);
    try {
      const args = lift(shape.params, JSON.parse(input.text)) as unknown[];
      const result = await this.port.run(BigInt(environment.id), scope, () =>
        values.fromGuest(() => {
          const fn = this.port.exports?.[key]?.[spelling];
          if (typeof fn !== "function")
            throw new Error(`The component does not export ${key}#${spelling}`);
          return fn(...values.guestArguments(args));
        }),
      );
      return check(this.contract, slot, false, encode(result, VALUE_LIMIT, shape.result));
    } finally {
      scope.cancel();
      release();
    }
  }

  /** The admitted call's execution context for one guest entry. */
  private static entered(context: Context): Context {
    return new Context(
      context.session,
      context.root,
      context.invocation,
      context.session.kernel.command(KernelOp.EnterExecution, {
        call: context.invocation!.id,
        context: context.frame || undefined,
      }),
    );
  }

  /** The instance's incoming-HTTP bridge from the HTTP adapter (`wasi.ts`). */
  ingressBridge: IngressBridge | undefined;

  /**
   * The host-local incoming HTTP service of a `wasi:http/handler` export. It has no contract
   * signature, so no WIT dependency can bind it, and its method accepts only an exchange issued by
   * `Host.handleHttp`. One request is one admitted callback: the response head is published as
   * soon as the handler returns it, while the callback stays admitted until the task's real exit
   * and the end of the response delivery.
   */
  private ingress(
    contract: string,
    description: Description,
    create: (service: string) => (declaration: Declaration) => Promise<Scope>,
    admit: () => Promise<() => void>,
  ): ServiceDefinition {
    if (contract !== HTTP_HANDLER) throw new Error(`Unsupported incoming HTTP export ${contract}`);
    const service = this.graph.javascriptNames["handler"];
    const method = this.graph.javascriptNames["handle"];
    if (!service || !method) throw new Error("Missing prepared incoming HTTP handler names");
    const declared = description.services[service];
    if (!declared?.methods.includes(method))
      throw new Error(`The Orb exports ${HTTP_HANDLER} without a ${service}.${method} handler`);
    return {
      [declareFactory]: create(service),
      lifetime: declared.lifetime,
      linger: declared.linger,
      [admitFactory]: admit,
      methods: {
        handle: async (environment: Scope, context: Context, value: unknown): Promise<void> => {
          const exchange = IngressExchange.claim(value);
          const bridge = this.ingressBridge;
          if (!bridge) throw new Error("This browser host has no incoming HTTP bridge");
          const scope = environment.exchange({
            context: ComponentRevision.entered(context),
          });
          let failure: unknown = new Error("the HTTP handler callback ended");
          const release = await this.gate.enter(undefined, scope.signal);
          try {
            const { result, finished } = this.port.start(
              BigInt(environment.id),
              scope,
              async () => {
                const handle = this.port.exports?.[HTTP_HANDLER]?.[method];
                if (typeof handle !== "function")
                  throw new Error(`The component does not export ${HTTP_HANDLER}#${method}`);
                try {
                  return { ok: await handle(bridge.request(exchange.own())) };
                } catch (error) {
                  // The handler's typed `err` lane refuses the request; anything else is a trap.
                  if (
                    error === null ||
                    typeof error !== "object" ||
                    !Object.hasOwn(error, "payload")
                  )
                    throw error;
                  return { err: (error as { payload: unknown }).payload };
                }
              },
            );
            let delivery: Promise<void> | undefined;
            try {
              const outcome = await result;
              if ("ok" in outcome) delivery = bridge.respond(outcome.ok, exchange);
              else exchange.refuse(outcome.err as never);
            } finally {
              await finished;
            }
            await delivery;
            failure = undefined;
          } catch (error) {
            failure = error;
            throw error;
          } finally {
            exchange.settle(failure);
            scope.cancel();
            release();
          }
        },
      },
    } as AdaptedService;
  }

  /** Build callback definitions from the checked plan. */
  definition(
    identity: Pick<OrbDefinition, "name" | "apiVersion">,
    installation: InstallationPlan,
    description: Description,
  ): OrbDefinition {
    const checked = query<CheckedImplementation>(this.contract, {
      kind: "implementation",
      name: identity.name,
      apiVersion: identity.apiVersion,
      value: description,
      bindings: installation.bindings,
    });
    const { plan } = checked;
    const graph = this.graph;
    const compile = shapes(graph);
    const convert = converters(graph);
    const names = graph.javascriptNames;
    const exported = (contract: string, method: string): [string, WitFunction] => {
      const entry = graph.exports.find((item) => item.contract === contract);
      const found = Object.entries(entry?.functions ?? {}).find(([name]) => names[name] === method);
      if (!found) throw new Error(`Missing prepared signature for ${contract}#${method}`);
      return found;
    };
    for (const hook of checked.hooks.hooks) {
      this.hooks.set(hook.alias, hook);
      this.signatures.set(hook.slot, compile(exported(hook.name, hook.method)[1]));
    }
    for (const [binding, service] of Object.entries(installation.importServices)) {
      if (checked.hooks.contributions.some((contribution) => contribution.binding === binding))
        continue;
      this.imports.set(binding, service);
      const provider = installation.bindings[binding];
      const requirement =
        provider?.kind === "orb"
          ? plan.orb.requires.find((item) => item.orb === provider.name)
          : undefined;
      const signature_ =
        requirement && Object.hasOwn(requirement.signatures, service)
          ? requirement.signatures[service]
          : undefined;
      if (signature_) this.importSignatures.set(binding, signature_);
      const selected = graph.imports.find((item) => item.binding === binding);
      if (!selected?.functions) throw new Error("Missing prepared import signatures");
      const methods = new Map<string, { name: string; slot: number }>();
      for (const [name, fn] of Object.entries(selected.functions)) {
        const spelling = names[name];
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
      if (!this.source) {
        if (service === null) scope.declareDefaults(declaration);
        return scope;
      }
      const authority = scope.exchange({ declaration });
      try {
        await this.callback(scope.id, authority, (lifecycle) =>
          lifecycle.create!(BigInt(scope.id), service ?? undefined),
        );
      } finally {
        authority.cancel();
      }
      return scope;
    };
    const admit = () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2)));
    const services: Record<string, ServiceDefinition> = {};
    for (const service of checked.services) {
      const { interface: binding, alias } = service;
      const functions = graph.exports.find((item) => item.binding === binding)?.functions;
      services[service.name] = {
        [signature]: service.signature,
        [declareFactory]: create(alias),
        lifetime: service.lifetime,
        linger: service.linger,
        [admitFactory]: admit,
        methods: Object.fromEntries(
          Object.entries(service.methods).map(([method, spelling]) => {
            const slot = this.contract.bind(binding, method, true);
            const fn = functions?.[method];
            if (!fn) throw new Error(`Missing prepared signature for ${binding}#${method}`);
            const shape = compile(fn);
            const values = convert(fn as GraphFunction);
            this.signatures.set(slot, shape);
            const invoke = (environment: Scope, context: Context, input: Encoded) =>
              this.invoke(
                binding,
                spelling,
                values,
                slot,
                shape,
                environment,
                environment.exchange({
                  context: ComponentRevision.entered(context),
                }),
                input,
              );
            return [
              method,
              bindCaptured(
                async (environment: Scope, context: Context, ...args: unknown[]) => {
                  const input = check(
                    this.contract,
                    slot,
                    true,
                    encode(args, VALUE_LIMIT, shape.params),
                  );
                  const output = await invoke(environment, context, input);
                  return lift(shape.result, JSON.parse(output.text));
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
        admit,
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
      const [, fn] = exported(binding.name, binding.method);
      const shape = compile(fn);
      const values = convert(fn as GraphFunction);
      this.signatures.set(binding.slot, shape);
      (contributes[binding.target] ??= {})[binding.name] = {
        priority: binding.priority,
        handle: async (context, input) => {
          if (!rootScope || this.scopes.get(rootScope.id) !== rootScope)
            throw new Error("Contribution Orb scope is unavailable");
          const arguments_ = check(
            this.contract,
            binding.slot,
            true,
            encode([input], VALUE_LIMIT, shape.params),
          );
          const result = await this.invoke(
            binding.name,
            binding.method,
            values,
            binding.slot,
            shape,
            rootScope,
            rootScope.exchange({ context: ComponentRevision.entered(context) }),
            arguments_,
          );
          const value = lift(
            shape.result,
            JSON.parse(checkedResult(this.contract, binding.slot, result.text).text),
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
        if (!scope.validators.length) return;
        const release = await this.gate.enter();
        try {
          // Kernel state is JSON; the guest validates the same text it would read.
          const text = encode(value).text;
          for (const callback of scope.validators) {
            const authority = scope.exchange({ validation: true });
            try {
              await this.callback(scope.id, authority, (lifecycle) =>
                lifecycle.validate!(callback, text),
              );
            } finally {
              authority.cancel();
            }
          }
        } finally {
          release();
        }
      },
      [admitFactory]: admit,
      [releaseEnvironment]: async (environment: Environment) => {
        const scope = this.scopes.get(environment.id);
        if (!scope) return;
        this.scopes.delete(scope.id);
        if (!this.source) return;
        const release = await this.gate.enter();
        try {
          // A quarantined instance is never entered again, not even for pure cleanup.
          if (!this.port.usable) return;
          await this.port.run(BigInt(scope.id), NO_AUTHORITY, () =>
            this.lifecycle().release!(BigInt(scope.id)),
          );
        } finally {
          release();
        }
      },
      [disposeOrb]: async () => {
        if (this.#closed) return;
        if (this.scopes.size) throw new Error("Component still owns physical scopes");
        await this.port.close();
        this.#closed = true;
      },
    } as AdaptedOrb;
  }
}
