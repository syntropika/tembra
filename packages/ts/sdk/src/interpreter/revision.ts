import type { Description } from "./types.ts";
import type { Declaration } from "../authoring/declaration.ts";
import type { OrbDefinition, ServiceDefinition } from "../authoring/types.ts";
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

import { check, encode, packet, checkedResult, type Encoded } from "./codec.ts";
import { Gate } from "./gate.ts";
import { Scope } from "./scope.ts";
import {
  query,
  type ContractPort,
  type ServiceBinding,
  type InstallationPlan,
  type InterpreterPort,
  type ExecutionScope,
} from "./types.ts";

/** One isolated revision translates callbacks into ordinary SDK declarations and calls. */
export class Revision {
  readonly gate = new Gate();
  readonly imports = new Map<string, string>();
  readonly importSignatures = new Map<string, string>();
  readonly importMethods = new Map<string, Map<string, { name: string; slot: number }>>();
  readonly scopes = new Map<number, Scope>();
  active = false;
  #closed = false;

  /** Both transports share the resolved Rust contract; the SDK remains the sole lifecycle adapter. */
  constructor(
    readonly port: InterpreterPort,
    readonly contract: ContractPort,
    readonly timeout: number,
    readonly io: (
      request: import("./host-protocol.ts").IORequest,
    ) => Promise<unknown> = async () => {
      throw new Error("No I/O provider is attached");
    },
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

  /** Build callback definitions from WIT rather than asking consumers to repeat service metadata. */
  definition(
    identity: Pick<OrbDefinition, "name" | "apiVersion">,
    installation: InstallationPlan,
  ): OrbDefinition {
    const { plan, importServices: imports } = installation;
    const checked = query<ServiceBinding[]>(this.contract, {
      kind: "implementation",
      name: identity.name,
      apiVersion: identity.apiVersion,
      value: JSON.parse(this.port.description()) as Description,
    });
    const description = query<{
      imports: { binding: string; functions?: Record<string, unknown> }[];
      javascriptNames: Record<string, string>;
    }>(this.contract, { kind: "describe" });
    for (const [binding, service] of Object.entries(imports)) {
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
      for (const name of Object.keys(selected.functions)) {
        const spelling = description.javascriptNames[name];
        if (!spelling || methods.has(spelling)) throw new Error("Ambiguous prepared import method");
        methods.set(spelling, { name, slot: this.contract.bind(binding, name, false) });
      }
      this.importMethods.set(binding, methods);
    }
    const create = (service: string | null) => async (declaration: Declaration) => {
      const scope = new Scope(this, declaration, service);
      this.scopes.set(scope.id, scope);
      await this.declare(
        JSON.stringify({ kind: "create", environment: scope.id, service }),
        scope.exchange({ declaration }),
      );
      return scope;
    };
    const services: Record<string, ServiceDefinition> = {};
    for (const service of checked) {
      const { interface: binding, alias } = service;
      services[service.name] = {
        [signature]: service.signature,
        [declareFactory]: create(alias),
        lifetime: service.lifetime,
        linger: service.linger,
        [admitFactory]: () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2))),
        methods: Object.fromEntries(
          Object.entries(service.methods).map(([method, spelling]) => {
            const slot = this.contract.bind(binding, method, true);
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
                  { kind: "invoke", environment: environment.id, service: alias, method: spelling },
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
                  const input = check(this.contract, slot, true, encode(args));
                  return JSON.parse((await invoke(environment, context, input)).text);
                },
                service.signature,
                invoke,
              ),
            ];
          }),
        ),
      } as AdaptedService;
    }
    return {
      name: identity.name,
      apiVersion: identity.apiVersion,
      services,
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
              packet({ kind: "validate", callback }, "value", encode(value)),
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
