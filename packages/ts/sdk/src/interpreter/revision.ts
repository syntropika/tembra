import { checkShape, type Description } from "./shape.ts";
import type { Declaration } from "../authoring/declaration.ts";
import type { OrbDefinition, ServiceDefinition } from "../authoring/types.ts";
import { Lifetime } from "../bridge/identifiers.ts";
import { ContextKey } from "../context-key.ts";
import {
  declareFactory,
  validateEnvironment,
  admitFactory,
  disposeOrb,
  releaseEnvironment,
  type AdaptedOrb,
  type AdaptedService,
} from "@syntropika/tembra-sdk/internal/adapters";
import type { Context } from "../runtime/context.ts";
import type { Environment } from "../runtime/records.ts";
import { declare, dispatch, type Scope as ExchangeScope } from "./exchange.ts";
import { Gate } from "./gate.ts";
import { Scope } from "./scope.ts";
import {
  camel,
  query,
  type ContractPort,
  type InterpreterOrbOptions,
  type InterpreterPort,
} from "./types.ts";

const stack = ContextKey.text("tembra.executor.wasm.stack");
interface Plan {
  orb: {
    services: { name: string; methods: string[] }[];
    requires: {
      orb: string;
      apiVersion: number;
      services: Record<string, string[]>;
      hooks: string[];
    }[];
  };
}

/** One isolated revision translates callbacks into ordinary SDK declarations and calls. */
export class Revision {
  readonly gate = new Gate();
  readonly imports = new Map<string, string>();
  readonly scopes = new Map<number, Scope>();
  readonly identity = globalThis.crypto.randomUUID();
  active = false;
  #closed = false;

  /** Both transports share the resolved Rust contract; the SDK remains the sole lifecycle adapter. */
  constructor(
    readonly port: InterpreterPort,
    readonly contract: ContractPort,
    readonly timeout: number,
  ) {}

  /** Run pure synchronous guest work only while its enclosing factory owns admission. */
  async declare(packet: unknown, scope: ExchangeScope): Promise<unknown> {
    this.active = true;
    try {
      return await declare(this.port, packet, scope);
    } finally {
      this.active = false;
    }
  }

  /** Hold admission until Wasmtime reports physical settlement, including after observer cancellation. */
  async execute(packet: unknown, scope: ExchangeScope): Promise<unknown> {
    const release = await this.gate.enter(undefined, scope.signal);
    this.active = true;
    try {
      scope.check();
      return await dispatch(this.port, packet, scope);
    } finally {
      this.active = false;
      release();
    }
  }

  /** Build callback definitions from WIT rather than asking consumers to repeat service metadata. */
  definition(options: InterpreterOrbOptions): OrbDefinition {
    const bindings = options.bindings ?? {};
    query(this.contract, { kind: "profile", bindings });
    const plan = query<Plan>(this.contract, {
      kind: "plan",
      name: options.name,
      apiVersion: options.apiVersion,
      bindings,
    });
    const description = JSON.parse(this.port.description()) as Description;
    checkShape(description, options.name, options.apiVersion, plan.orb.services);
    const graph = query<{ imports: { binding: string; contract: string }[] }>(this.contract, {
      kind: "describe",
    });
    for (const item of graph.imports) {
      const provider = bindings[item.binding];
      if (provider?.kind === "orb")
        this.imports.set(item.binding, `${provider.name}::${item.contract}`);
    }
    const create = (service: string | null) => async (declaration: Declaration) => {
      const scope = new Scope(this, declaration, service);
      this.scopes.set(scope.id, scope);
      await this.declare(
        { kind: "create", environment: scope.id, service },
        scope.exchange({ declaration }),
      );
      return scope;
    };
    const services: Record<string, ServiceDefinition> = {};
    for (const service of plan.orb.services) {
      const binding = service.name.slice(options.name.length + 2);
      const alias = camel(binding.split("/").at(-1)!.split("@")[0]!);
      const implementation = description.services[alias]!;
      services[service.name] = {
        [declareFactory]: create(alias),
        lifetime: implementation.lifetime ?? Lifetime.Referenced,
        linger: implementation.linger ?? 0,
        [admitFactory]: () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2))),
        methods: Object.fromEntries(
          service.methods.map((method) => [
            method,
            async (environment: Scope, context: Context, ...args: unknown[]) => {
              context.check();
              const previous = context.value(stack) ?? "";
              if (previous.split(",").includes(this.identity))
                throw new Error("Causal reentry into the same interpreter is unsupported");
              const entered = context.with(stack, `${previous},${this.identity}`);
              query(this.contract, { kind: "arguments", binding, method, value: args });
              const result = await this.execute(
                {
                  kind: "invoke",
                  environment: environment.id,
                  service: alias,
                  method: camel(method),
                  args,
                },
                environment.exchange({ context: entered }),
              );
              query(this.contract, { kind: "result", binding, method, value: result });
              return result;
            },
          ]),
        ),
      } as AdaptedService;
    }
    return {
      name: options.name,
      apiVersion: options.apiVersion,
      services,
      requires: Object.fromEntries(
        plan.orb.requires.map((requirement) => [requirement.orb, requirement]),
      ),
      [declareFactory]: create(null),
      [validateEnvironment]: async (environment: Environment, value: unknown) => {
        const scope = this.scopes.get(environment.id);
        if (!scope) throw new Error("Validator scope is gone");
        const release = await this.gate.enter();
        try {
          for (const callback of scope.validators)
            await this.declare(
              { kind: "validate", callback, value },
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
        this.contract.free?.();
        this.#closed = true;
      },
    } as AdaptedOrb;
  }
}
