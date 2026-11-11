/**
 * One installed compiled component revision, adapted to ordinary SDK Orb definitions.
 *
 * The checked contract plans services, hooks, contributions and incoming HTTP exactly as for
 * native hosts; a compiled component is described from its contract and its artifact's roles
 * (contributions, declared hooks, service lifetimes). Every guest entry is one callback admitted
 * by the revision gate and served by the component port, whose instrumented task observation
 * keeps admission and import attribution until physical completion. Kernel state, root resources,
 * declared hooks and the call's context are served by `kernel.ts`.
 */
import type { Declaration } from "../../authoring/declaration.ts";
import type { Contribution, OrbDefinition, ServiceDefinition } from "../../authoring/types.ts";
import type { Lifetime } from "../../bridge/identifiers.ts";
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
import { HTTP_HANDLER, IngressExchange } from "../http-ingress.ts";
import { declaredHooks } from "../revision.ts";
import {
  query,
  type CheckedImplementation,
  type ContractPort,
  type Description,
  type InstallationPlan,
  type ReferenceBinding,
} from "../types.ts";
import { lift, shapes, type Signature, type WitFunction, type WitTypes } from "../wit-shape.ts";
import { viewMethod } from "./host.ts";
import { ComponentKernel, isKernel, type LogObserver } from "./kernel.ts";
import type { ComponentPort } from "./port.ts";
import { ComponentScope, type BoundMethod, type CallbackScope, type ScopeOwner } from "./scope.ts";
import { converters, type FunctionValues, type GraphFunction, type TypeGraph } from "./values.ts";
import type { IngressBridge } from "./wasi-http.ts";

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
    /** Checked reference views; artifact metadata alone declares them. */
    references: Readonly<Record<string, ReferenceBinding>>;
  };

/**
 * The roles a compiled artifact's metadata gives its exports, by interface identity: the hooks
 * it contributes to with their priority, the hooks it declares with their composition, and the
 * lifetime and linger of its services' objects.
 */
export interface CompiledRoles {
  readonly contributions: Readonly<Record<string, number>>;
  readonly hooks?: Readonly<Record<string, "series" | "waterfall" | "bail">>;
  readonly services?: Readonly<Record<string, { lifetime: Lifetime; linger_ms: number }>>;
}

/**
 * A compiled component's description: one service per callable export interface, with the
 * lifetime and linger its policy names, and its declared hooks under their export's alias, as
 * the native executor describes it. Contribution callbacks and the kernel's own callbacks
 * (`tembra:guest`) are answered to their callers, never offered as services.
 */
export function compiledDescription(
  graph: Described,
  identity: { name: string; apiVersion: number },
  roles: CompiledRoles,
): Description {
  const services: Description["services"] = {};
  const hooks: NonNullable<Description["hooks"]> = {};
  const { contributions, hooks: declared = {}, services: policies = {} } = roles;
  const spelling = (name: string) => {
    const value = graph.javascriptNames[name];
    if (value === undefined) throw new Error(`Missing WIT JavaScript name for ${name}`);
    return value;
  };
  for (const entry of graph.exports) {
    if (!entry.contract || !entry.functions || !Object.keys(entry.functions).length) continue;
    const name = entry.contract.slice(entry.contract.indexOf("/") + 1).split("@")[0]!;
    if (Object.hasOwn(declared, entry.contract)) {
      hooks[spelling(name)] = declared[entry.contract]!;
      continue;
    }
    if (Object.hasOwn(contributions, entry.contract) || isKernel(entry.contract)) continue;
    const policy = Object.hasOwn(policies, entry.contract) ? policies[entry.contract] : undefined;
    services[spelling(name)] = {
      methods: Object.keys(entry.functions).map(spelling),
      ...(policy ? { lifetime: policy.lifetime, linger: policy.linger_ms } : {}),
    };
  }
  return {
    name: identity.name,
    apiVersion: identity.apiVersion,
    services,
    hooks,
    contributes: { ...contributions },
  };
}

/** The admitted call's execution context for one guest entry. */
function entered(context: Context): Context {
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

export class ComponentRevision implements ScopeOwner {
  readonly gate = new Gate();
  readonly imports = new Map<string, string>();
  readonly importSignatures = new Map<string, string>();
  readonly importMethods = new Map<string, Map<string, BoundMethod>>();
  readonly views = new Map<string, Map<string, BoundMethod>>();
  readonly signatures = new Map<number, Signature>();
  readonly scopes = new Map<number, ComponentScope>();
  /** The instance's incoming-HTTP bridge from the HTTP adapter (`wasi.ts`). */
  ingressBridge: IngressBridge | undefined;
  /** The kernel's interfaces (`tembra:guest`) and the hooks this Orb declares. */
  readonly kernel: ComponentKernel;
  #closed = false;

  constructor(
    readonly port: ComponentPort,
    readonly contract: ContractPort,
    readonly graph: Described,
    readonly timeout: number,
    /** The Orb's name and where its `tembra:guest/context` log records go. */
    log: { readonly orb: string; readonly observer?: LogObserver } = { orb: "" },
  ) {
    this.kernel = new ComponentKernel(this, log.orb, log.observer);
  }

  /** Call one typed export under an admitted callback and answer checked result bytes. */
  private async invoke(
    key: string,
    spelling: string,
    values: FunctionValues,
    slot: number,
    shape: Signature,
    environment: ComponentScope,
    scope: CallbackScope,
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
    create: (service: string) => (declaration: Declaration) => Promise<ComponentScope>,
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
        handle: async (
          environment: ComponentScope,
          context: Context,
          value: unknown,
        ): Promise<void> => {
          const exchange = IngressExchange.claim(value);
          const bridge = this.ingressBridge;
          if (!bridge) throw new Error("This browser host has no incoming HTTP bridge");
          const scope = environment.exchange(entered(context));
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
    const exported = (contract: string, method: string): WitFunction => {
      const entry = graph.exports.find((item) => item.contract === contract);
      const found = Object.entries(entry?.functions ?? {}).find(([name]) => names[name] === method);
      if (!found) throw new Error(`Missing prepared signature for ${contract}#${method}`);
      return found[1];
    };
    // A declared hook runs its plan through the kernel, its values checked by its callback.
    this.kernel.declareHooks(checked.hooks.hooks, (hook) =>
      compile(exported(hook.name, hook.method)),
    );
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
      const view = graph.references[binding];
      if (view) {
        // Only checked methods are callable; the acquisition result is never data.
        const methods = new Map<string, BoundMethod>();
        for (const [name, provider] of Object.entries(view.methods)) {
          const fn = selected.functions[name];
          if (!fn) throw new Error(`Missing prepared signature for ${binding}#${name}`);
          const slot = this.contract.bind(binding, name, false);
          this.signatures.set(slot, compile(viewMethod(fn)));
          methods.set(name, { name: provider, slot });
        }
        this.views.set(binding, methods);
        continue;
      }
      const methods = new Map<string, BoundMethod>();
      for (const [name, fn] of Object.entries(selected.functions)) {
        const spelling = names[name];
        if (!spelling || methods.has(spelling)) throw new Error("Ambiguous prepared import method");
        const slot = this.contract.bind(binding, name, false);
        this.signatures.set(slot, compile(fn));
        methods.set(spelling, { name, slot });
      }
      this.importMethods.set(binding, methods);
    }
    let rootScope: ComponentScope | undefined;
    // A compiled component has no factory code: a factory issues the host scope and declares
    // what the component's kernel callbacks describe for it. The factory's admission holds the
    // gate, so those callbacks enter the component directly.
    const create =
      (service: string | null, contract?: string) => async (declaration: Declaration) => {
        const scope = new ComponentScope(this, declaration, service);
        if (service === null) rootScope = scope;
        this.scopes.set(scope.id, scope);
        await this.kernel.declare(scope, declaration, service, contract);
        return scope;
      };
    const admit = () => this.gate.enter(Math.max(1, Math.floor(this.timeout / 2)));
    const services: Record<string, ServiceDefinition> = {};
    for (const service of checked.services) {
      const { interface: binding, alias } = service;
      const functions = graph.exports.find((item) => item.binding === binding)?.functions;
      services[service.name] = {
        [signature]: service.signature,
        [declareFactory]: create(alias, binding),
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
            const invoke = (environment: ComponentScope, context: Context, input: Encoded) =>
              this.invoke(
                binding,
                spelling,
                values,
                slot,
                shape,
                environment,
                environment.exchange(entered(context)),
                input,
              );
            return [
              method,
              bindCaptured(
                async (environment: ComponentScope, context: Context, ...args: unknown[]) => {
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
    const contributes: Record<string, Record<string, Contribution>> = {};
    for (const binding of checked.hooks.contributions) {
      const fn = exported(binding.name, binding.method);
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
            rootScope.exchange(entered(context)),
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
      hooks: declaredHooks(checked.hooks.hooks),
      contributes,
      requires: Object.fromEntries(
        plan.orb.requires.map((requirement) => [
          requirement.orb,
          { ...requirement, [signatures]: requirement.signatures },
        ]),
      ),
      [isolatedRevision]: true,
      [declareFactory]: create(null),
      // A reload adopts a retained state only once the component validated it.
      [validateEnvironment]: async (environment: Environment, value: unknown) => {
        const scope = this.scopes.get(environment.id);
        if (!scope) throw new Error("Validator scope is gone");
        const release = await this.gate.enter();
        try {
          await this.kernel.validate(scope, value);
        } finally {
          release();
        }
      },
      [admitFactory]: admit,
      [releaseEnvironment]: async (environment: Environment) => {
        this.kernel.release(environment.id);
        this.scopes.delete(environment.id);
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
