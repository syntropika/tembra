import { EnvironmentPhase, KernelOp } from "../bridge/protocol.ts";
import { Errors } from "../errors.ts";

import { refuse } from "../error.ts";
import type { Context } from "../runtime/context.ts";
import type { Environment } from "../runtime/records.ts";
import type { Session } from "../runtime/session.ts";
import type { Readable, State, StateOptions } from "../state.ts";
import { StateBinding, validate } from "../state.ts";
import { serviceAcquisition } from "./services.ts";
import type {
  AcquisitionOptions,
  Completion,
  DynamicReference,
  EventSource,
  ListenerOptions,
  Resource,
  ResourceOptions,
  TaskScope,
  ServiceDefinition,
  ServiceReference,
} from "./types.ts";

/** Available only during its synchronous factory pass. Issued handles remain usable afterwards. */
export class Declaration {
  /** Bind declarations to one synchronous factory pass. */
  constructor(
    readonly session: Session,
    readonly environment: Environment,
  ) {}

  /** Reject declarations after the factory returns. */
  private check(): void {
    if (this.environment.phase !== EnvironmentPhase.Declaring)
      refuse(Errors.Invalid.FactoryExpired, "Declarations must be synchronous");
  }

  /** Read the identity that owns this scope. */
  get principal(): string {
    return this.session.principalFor(this.environment.object?.id ?? 0);
  }

  /** Read the shared value created by the orb factory. */
  get orb(): unknown {
    return this.environment.orb.scope?.value;
  }

  /** Read the object key, or the default key outside an object. */
  get key(): readonly string[] {
    return this.environment.object?.address.key ?? ["main"];
  }

  /** Declare object state with local validation and versioned migration. */
  state<T>(initial: T, options: StateOptions<T> = {}): State<T> {
    this.check();
    const environment = this.environment;
    const object = environment.object;
    if (!object || environment.state)
      refuse(Errors.Invalid.State, "An object declares exactly one state");
    const version = options.version ?? 1;
    if (!Number.isInteger(version) || version <= 0) refuse(Errors.Invalid.StateVersion);
    let binding = object.binding;
    if (!binding && object.live)
      binding = new StateBinding(this.session, object.id, validate(initial, options));
    if (!binding) {
      const value = validate(initial, options);
      const id = this.session.values.put(value);
      try {
        this.session.kernel.command(KernelOp.StateInit, { object: object.id, value: id, version });
      } catch (error) {
        this.session.values.drop(id);
        throw error;
      }
      object.binding = binding = new StateBinding(this.session, object.id);
    }
    const handle = binding.view(options);
    environment.state = { initial, options, binding };
    return handle;
  }

  /** Orb factories cannot depend on their own services; object factories may use sibling services. */
  get<const Service extends ServiceDefinition>(
    service: Service,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ): ServiceReference<Service>;
  get(
    service: string,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ): DynamicReference;
  get(
    service: string | ServiceDefinition,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
  ): DynamicReference {
    this.check();
    const acquisition = serviceAcquisition(service, options);
    const requirement = this.session.scopes.requirement(this.environment, acquisition.name);
    const resolved = requirement.service;
    if (typeof service !== "string" && resolved !== acquisition.name)
      refuse(Errors.Invalid.Service, "A typed dependency must resolve its declared service name");
    if (!this.environment.object && this.environment.orb.definition.services?.[resolved])
      refuse(Errors.Invalid.DependencyCycle);
    const admitted = serviceAcquisition(service, {
      ...options,
      ...(requirement.apiVersion ? { apiVersion: requirement.apiVersion } : {}),
      ...(requirement.methods ? { methods: requirement.methods } : {}),
      principal: options.principal ?? this.principal,
    });
    const identity = JSON.stringify([resolved, key, options.principal ?? this.principal]);
    const existing = this.environment.dependencies.get(identity);
    if (existing) return existing;
    const reference = this.session.handles.hold(resolved, key, admitted.options);
    if (this.environment.object) {
      try {
        this.session.kernel.command(KernelOp.Dependency, {
          object: this.environment.object.id,
          reference: this.session.handles.reference(reference).id,
        });
      } catch (error) {
        reference.release();
        throw error;
      }
    }
    this.environment.dependencies.set(identity, reference);
    return reference;
  }

  /** Declare an owned resource, or access a named host resource. */
  resource<T>(name: string, options?: ResourceOptions<T>): Resource<T> {
    this.check();
    if (!options) return this.session.scopes.hostResource<T>(name);
    if (this.environment.resources.has(name)) refuse(Errors.Invalid.DuplicateResource, name);
    const resource = this.session.resources.declare(this.environment, name, options);
    this.environment.resources.set(name, resource);
    return resource.handle;
  }

  /** Register work that ends with its declaring scope. */
  task(start: (scope: TaskScope) => Completion<void>): void {
    this.check();
    if (typeof start !== "function") refuse(Errors.Invalid.Task);
    this.environment.tasks.push({ start });
  }

  /** Declare a bounded listener that starts after activation. */
  on<T>(
    name: string,
    source: EventSource<T> | (() => EventSource<T>),
    handle: (context: Context, value: T) => unknown,
    options: ListenerOptions = {},
  ): void {
    this.check();
    if (!this.session.kernel.features.subscriptions) refuse(Errors.Invalid.SubscriptionsDisabled);
    if (options.maxBytes !== undefined && !options.sizeOf)
      refuse(
        Errors.Invalid.SubscriptionSize,
        "A local byte budget requires an explicit sizeOf function",
      );
    if (!name || this.environment.listeners.some((listener) => listener.name === name))
      refuse(Errors.Invalid.DuplicateListener, name);
    this.environment.listeners.push({ name, source, handle, options });
  }

  /** Bind a declared hook to this scope. */
  hook(name: string): {
    /** Execute this hook under an issued operation context. */
    run(context: Context, input?: unknown): Promise<unknown>;
    /** Await one observation from this hook's original physical operation, even after cancellation.
     * A successful admission is consumed across all hook handles for that source invocation.
     * The scope retains principal/version, starts with empty metadata, and respects owner/host close.
     * Observer errors reject this promise; the consumer retains its primary outcome separately.
     * A root-only source rejects with forbidden/observation_source without consuming admission.
     * Dependent calls require exact open scopes and registered ancestors; missing scopes reject
     * with gone/scope and closing scopes with conflict/scope_closed. Ordinary calls are unchanged.
     */
    observe(source: Context, input?: unknown): Promise<void>;
  } {
    this.check();
    if (!this.session.kernel.features.hooks) refuse(Errors.Invalid.HooksDisabled);
    if (!this.environment.orb.definition.hooks?.[name]) refuse(Errors.Invalid.Hook, name);
    const operation = this.session.kernel.command<number>(KernelOp.Allocate);
    return {
      /** Require an issued context before entering the declared hook. */
      run: (context, input) => this.session.orbs.runHook(this.environment, name, context, input),
      observe: (source, input) =>
        this.session.orbs.observe(this.environment, name, operation, source, input),
    };
  }
}

/** Expose state reads while the consuming reference remains valid. */
export function consumerState<T>(
  binding: StateBinding<T>,
  check: () => void,
  cleanups: Set<() => void>,
): Readable<T> {
  return {
    /** Read state while the consuming reference remains valid. */
    get value() {
      check();
      return binding.value;
    },
    /** Create a selection tied to this reference's cleanup. */
    select(compute) {
      check();
      return binding.select(compute, check, cleanups);
    },
    /** Detach this observer when the consuming reference is released. */
    subscribe(listener) {
      check();
      const detach = binding.subscribe((value) => {
        try {
          check();
        } catch {
          return;
        }
        listener(value);
      });
      let active = true;
      const cleanup = () => {
        if (!active) return;
        active = false;
        cleanups.delete(cleanup);
        detach();
      };
      cleanups.add(cleanup);
      try {
        check();
      } catch {
        cleanup();
      }
      return cleanup;
    },
  };
}
