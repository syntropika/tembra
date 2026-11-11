/**
 * One installed compiled component revision, adapted to ordinary SDK Orb definitions.
 *
 * The checked contract plans services, contributions and incoming HTTP exactly as for native
 * hosts; a compiled component is described from its contract alone. Every guest entry is one
 * callback admitted by the revision gate and served by the component port, whose instrumented
 * task observation keeps admission and import attribution until physical completion.
 */
import type { Declaration } from "../../authoring/declaration.ts";
import type { Contribution, OrbDefinition, ServiceDefinition } from "../../authoring/types.ts";
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
  type CapturedDelivery,
  type ResourceCapability,
  type AdaptedOrb,
  type AdaptedService,
} from "#tembra-adapters";
import { Context } from "../../runtime/context.ts";
import type { Environment } from "../../runtime/records.ts";
import { check, checkedResult, encode, VALUE_LIMIT, Encoded } from "../codec.ts";
import { Gate } from "../gate.ts";
import { HTTP_HANDLER, IngressExchange } from "../http-ingress.ts";
import {
  query,
  type CheckedImplementation,
  type ContractPort,
  type Description,
  type InstallationPlan,
  type ReferenceBinding,
} from "../types.ts";
import { lift, shapes, type Signature, type WitFunction, type WitTypes } from "../wit-shape.ts";
import {
  ResourceOwner,
  resourceRoles,
  resourceDataFunction,
  resourceExport,
  type ResourceRoles,
} from "./resources.ts";
import { issueByteStream } from "./streams.ts";
import { viewMethod } from "./host.ts";
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

/** A compiled component's description: one service per callable export interface. */
export function compiledDescription(
  graph: Described,
  identity: { name: string; apiVersion: number },
  contributions: Readonly<Record<string, number>>,
): Description {
  const services: Description["services"] = {};
  const spelling = (name: string) => {
    const value = graph.javascriptNames[name];
    if (value === undefined) throw new Error(`Missing WIT JavaScript name for ${name}`);
    return value;
  };
  for (const entry of graph.exports) {
    if (!entry.contract || !entry.functions || !Object.keys(entry.functions).length) continue;
    if (Object.hasOwn(contributions, entry.contract)) continue;
    const name = entry.contract.slice(entry.contract.indexOf("/") + 1).split("@")[0]!;
    services[spelling(name)] = {
      methods: Object.keys(entry.functions).map(spelling),
    };
  }
  return {
    name: identity.name,
    apiVersion: identity.apiVersion,
    services,
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
  readonly importedResources = new Set<ResourceCapability>();
  readonly #issuedResources = new Map<number, Set<ResourceOwner>>();
  /** The instance's incoming-HTTP bridge from the HTTP adapter (`wasi.ts`). */
  ingressBridge: IngressBridge | undefined;
  #closed = false;

  constructor(
    readonly port: ComponentPort,
    readonly contract: ContractPort,
    readonly graph: Described,
    readonly timeout: number,
  ) {
    port.wasi.retired.addEventListener(
      "abort",
      () => {
        for (const owner of this.importedResources) owner.close();
        for (const owners of this.#issuedResources.values())
          for (const owner of owners) owner.close();
      },
      { once: true },
    );
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
    roles: ResourceRoles = {},
    receiver?: ResourceCapability,
    retain?: (physical: Promise<void>) => void,
  ): Promise<Encoded | CapturedDelivery> {
    const unlock = await this.gate.enter(undefined, scope.signal);
    let released = false;
    const release = () => {
      if (released) return;
      released = true;
      unlock();
    };
    let continued = false;
    const issued: ResourceOwner[] = [];
    try {
      if (!!roles.receiver !== !!receiver)
        throw new Error("Resource receiver differs from signature");
      const borrowed = roles.receiver && receiver?.redeem(this, environment.id, roles.receiver);
      const args = lift(shape.params, JSON.parse(input.text)) as unknown[];
      const call = () => {
        const exports = this.port.exports?.[key];
        if (!exports) throw new Error(`The component does not export ${key}`);
        return resourceExport(
          exports,
          spelling,
          roles,
          borrowed || undefined,
          values.guestArguments(args),
        );
      };
      if (roles.stream) {
        const task = this.port.start(BigInt(environment.id), scope, call);
        retain?.(task.physical);
        const finished = task.finished.finally(() => {
          scope.cancel();
          release();
        });
        continued = true;
        let owner: import("./streams.ts").ByteStreamOwner | undefined;
        try {
          const raw = await task.result;
          owner = issueByteStream(raw, finished, scope.signal ?? new AbortController().signal);
          const value = check(this.contract, slot, false, encode(0));
          // Publication releases only the exclusive entry turn. Its task, imports, scope,
          // deadline and active-group fuel remain owned until physical completion.
          task.park();
          release();
          return { value, capabilities: [owner], finished };
        } catch (error) {
          owner?.close();
          await finished.catch(() => undefined);
          throw error;
        }
      }
      const result = await this.port.run(
        BigInt(environment.id),
        scope,
        async () => {
          const result = await values.fromGuest(call);
          if (!roles.result) return result;
          return roles.result(result, (nominal, raw) => {
            if (typeof raw !== "object" || raw === null)
              throw new Error("Owned result is not a resource");
            const owner = new ResourceOwner(nominal, this, environment.id, raw, () => {
              return this.port.cleanup(async () => {
                const unlock = await this.gate.enter();
                try {
                  if (this.scopes.has(environment.id))
                    await this.port.destroy(() => {
                      const dispose = (raw as Disposable)[Symbol.dispose];
                      if (typeof dispose !== "function")
                        throw new Error("Resource has no destructor");
                      dispose.call(raw);
                    });
                } finally {
                  unlock();
                }
              });
            });
            let owners = this.#issuedResources.get(environment.id);
            if (!owners) this.#issuedResources.set(environment.id, (owners = new Set()));
            owners.add(owner);
            owner.whenClosed(() => owners.delete(owner));
            issued.push(owner);
            return issued.length - 1;
          });
        },
        retain,
      );
      const value = check(this.contract, slot, false, encode(result, VALUE_LIMIT, shape.result));
      return roles.owned ? { value, capabilities: issued } : value;
    } catch (error) {
      for (const owner of issued) owner.close();
      throw error;
    } finally {
      if (!continued) {
        scope.cancel();
        release();
      }
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
          let physical: Promise<void> | undefined;
          const release = await this.gate.enter(undefined, scope.signal);
          try {
            const task = this.port.start(BigInt(environment.id), scope, async () => {
              const handle = this.port.exports?.[HTTP_HANDLER]?.[method];
              if (typeof handle !== "function")
                throw new Error(`The component does not export ${HTTP_HANDLER}#${method}`);
              try {
                return { ok: await handle(bridge.request(exchange.own())) };
              } catch (error) {
                // The handler's typed `err` lane refuses the request; anything else is a trap.
                if (error === null || typeof error !== "object" || !Object.hasOwn(error, "payload"))
                  throw error;
                return { err: (error as { payload: unknown }).payload };
              }
            });
            physical = task.physical;
            const { result, finished } = task;
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
            await physical?.catch(() => undefined);
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
    if (checked.hooks.hooks.length) throw new Error("Compiled components declare no hooks");
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
        const roles = resourceRoles(graph, selected.contract ?? binding, {
          ...fn,
          name,
        } as GraphFunction);
        this.signatures.set(slot, compile(resourceDataFunction(fn as GraphFunction, roles)));
        methods.set(spelling, { name, slot, roles });
      }
      this.importMethods.set(binding, methods);
    }
    let rootScope: ComponentScope | undefined;
    // A compiled component has no factory code: a factory only issues the host scope.
    const create = (service: string | null) => async (declaration: Declaration) => {
      const scope = new ComponentScope(this, declaration, service);
      if (service === null) rootScope = scope;
      this.scopes.set(scope.id, scope);
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
            const roles = resourceRoles(graph, binding, { ...fn, name: method } as GraphFunction);
            const data = resourceDataFunction(fn as GraphFunction, roles);
            const shape = compile(data);
            const values = convert(data);
            this.signatures.set(slot, shape);
            const invoke = (
              environment: ComponentScope,
              context: Context,
              input: Encoded,
              receiver?: ResourceCapability,
              retain?: (physical: Promise<void>) => void,
            ) =>
              this.invoke(
                binding,
                spelling,
                values,
                slot,
                shape,
                environment,
                environment.exchange(entered(context)),
                input,
                roles,
                receiver,
                retain,
              );
            const invokeData = async (
              environment: ComponentScope,
              context: Context,
              args: readonly unknown[],
              retain?: (physical: Promise<void>) => void,
            ) => {
              const input = check(
                this.contract,
                slot,
                true,
                encode(args, VALUE_LIMIT, shape.params),
              );
              const output = await invoke(environment, context, input, undefined, retain);
              if (!(output instanceof Encoded)) {
                for (const capability of output.capabilities ?? []) capability.close();
                await output.finished;
                throw new Error("A data-only caller cannot receive a host capability");
              }
              return lift(shape.result, JSON.parse(output.text));
            };
            return [
              method,
              bindCaptured(
                (environment: ComponentScope, context: Context, ...args: unknown[]) =>
                  invokeData(environment, context, args),
                service.signature,
                invoke,
                invokeData,
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
          let physical: Promise<void> | undefined;
          let result: Encoded | CapturedDelivery;
          try {
            result = await this.invoke(
              binding.name,
              binding.method,
              values,
              binding.slot,
              shape,
              rootScope,
              rootScope.exchange(entered(context)),
              arguments_,
              {},
              undefined,
              (actual) => {
                physical = actual;
              },
            );
          } finally {
            // Contributions have no early capability lane: retain their owned operation
            // until actual guest/provider exit, including after a bounded failure.
            await physical?.catch(() => undefined);
          }
          if (!(result instanceof Encoded)) {
            for (const capability of result.capabilities ?? []) capability.close();
            throw new Error("A contribution cannot carry a host capability");
          }
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
      hooks: {},
      contributes,
      requires: Object.fromEntries(
        plan.orb.requires.map((requirement) => [
          requirement.orb,
          { ...requirement, [signatures]: requirement.signatures },
        ]),
      ),
      [isolatedRevision]: true,
      [declareFactory]: create(null),
      // A compiled component declares no state, so it has no state validators.
      [validateEnvironment]: async (environment: Environment) => {
        if (!this.scopes.has(environment.id)) throw new Error("Validator scope is gone");
      },
      [admitFactory]: admit,
      [releaseEnvironment]: async (environment: Environment) => {
        if (environment === environment.orb.scope) {
          for (const owner of this.importedResources) owner.close();
          this.importedResources.clear();
        }
        // Environment release owns any remaining exported instances. Drain while the
        // environment is still registered, so each guest destructor runs exactly once.
        const owners = this.#issuedResources.get(environment.id);
        for (const owner of owners ?? []) owner.close();
        // Closing schedules drops through the port's physical cleanup queue.
        await this.port.settled();
        this.#issuedResources.delete(environment.id);
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
