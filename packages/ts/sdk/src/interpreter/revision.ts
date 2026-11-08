import type { Description } from "./types.ts";
import type { Declaration } from "../authoring/declaration.ts";
import type { OrbDefinition, ServiceDefinition } from "../authoring/types.ts";
import type { Lifetime } from "../bridge/identifiers.ts";
import { ContextKey } from "../context-key.ts";
import {
  declareFactory,
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
import type { Context } from "../runtime/context.ts";
import type { Environment } from "../runtime/records.ts";

import { Gate } from "./gate.ts";
import { Scope } from "./scope.ts";
import {
  query,
  type ContractPort,
  type Registration,
  type InterpreterPort,
  type ExecutionScope,
} from "./types.ts";

const stack = ContextKey.text("tembra.executor.wasm.stack");
/** Mirrors the Rust `ServiceBinding` returned by the checked implementation query. */
interface ServiceBinding {
  name: string;
  interface: string;
  alias: string;
  /** Original WIT method names mapped to guest-facing JavaScript names. */
  methods: Record<string, string>;
  lifetime: Lifetime;
  linger: number;
  signature?: string;
}
interface Plan {
  orb: {
    requires: {
      orb: string;
      apiVersion: number;
      services: Record<string, string[]>;
      hooks: string[];
      signatures: Record<string, string>;
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
  async declare(packet: unknown, scope: ExecutionScope): Promise<unknown> {
    this.active = true;
    try {
      return await this.port.declare(JSON.stringify(packet), scope);
    } finally {
      scope.cancel();
      this.active = false;
    }
  }

  /** Hold admission until Wasmtime reports physical settlement, including after observer cancellation. */
  async execute(packet: unknown, scope: ExecutionScope): Promise<unknown> {
    const release = await this.gate.enter(undefined, scope.signal);
    this.active = true;
    try {
      scope.check();
      return await this.port.execute(JSON.stringify(packet), scope);
    } finally {
      scope.cancel();
      this.active = false;
      release();
    }
  }

  /** Build callback definitions from WIT rather than asking consumers to repeat service metadata. */
  definition(options: Registration): OrbDefinition {
    const bindings = options.bindings ?? {};
    query(this.contract, { kind: "profile", bindings });
    const plan = query<Plan>(this.contract, {
      kind: "plan",
      name: options.name,
      apiVersion: options.apiVersion,
      bindings,
    });
    const checked = query<ServiceBinding[]>(this.contract, {
      kind: "implementation",
      name: options.name,
      apiVersion: options.apiVersion,
      value: JSON.parse(this.port.description()) as Description,
    });
    const imports = query<Record<string, string>>(this.contract, {
      kind: "import-services",
      bindings,
    });
    for (const [binding, service] of Object.entries(imports)) this.imports.set(binding, service);
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
    for (const service of checked) {
      const { interface: binding, alias } = service;
      services[service.name] = {
        [signature]: service.signature,
        [declareFactory]: create(alias),
        lifetime: service.lifetime,
        linger: service.linger,
        [admitFactory]: () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2))),
        methods: Object.fromEntries(
          Object.entries(service.methods).map(([method, spelling]) => [
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
                  method: spelling,
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
        this.#closed = true;
      },
    } as AdaptedOrb;
  }
}
