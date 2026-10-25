import { Errors } from "../errors.ts";
import { EnvironmentPhase, KernelOp } from "../protocol.ts";

import { refuse } from "../error.ts";
import type { Readable, State, StateOptions } from "../state.ts";
import { StateBinding, validate } from "../state.ts";
import type {
  AcquisitionOptions,
  DynamicReference,
  EventSource,
  ListenerOptions,
  Resource,
  ResourceOptions,
  TaskScope,
  Completion,
} from "../types.ts";
import type { Context } from "./context.ts";
import type { Environment } from "./records.ts";
import type { Session } from "./session.ts";

/** Available only during its synchronous factory pass. Issued handles remain usable afterwards. */
export class Declaration {
  constructor(
    readonly session: Session,
    readonly environment: Environment,
  ) {}
  private check(): void {
    if (this.environment.phase !== EnvironmentPhase.Declaring)
      refuse(Errors.Invalid.FactoryExpired, "Declarations must be synchronous");
  }
  get principal(): string {
    return this.session.principalFor(this.environment.object?.id ?? 0);
  }
  get facet(): unknown {
    return this.environment.facet.scope?.value;
  }
  get key(): readonly string[] {
    return this.environment.object?.address.key ?? ["main"];
  }

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
    environment.state = { initial, options, handle, binding };
    return handle;
  }

  get(
    service: string,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
  ): DynamicReference {
    this.check();
    const requirement = this.session.requirement(this.environment, service);
    const resolved = requirement.service;
    const identity = JSON.stringify([resolved, key, options.principal ?? this.principal]);
    const existing = this.environment.dependencies.get(identity);
    if (existing) return existing;
    const reference = this.session.hold(resolved, key, {
      ...options,
      ...(requirement.apiVersion ? { apiVersion: requirement.apiVersion } : {}),
      ...(requirement.methods ? { methods: requirement.methods } : {}),
      principal: options.principal ?? this.principal,
    });
    if (this.environment.object) {
      try {
        this.session.kernel.command(KernelOp.Dependency, {
          object: this.environment.object.id,
          reference: this.session.reference(reference).id,
        });
      } catch (error) {
        reference.release();
        throw error;
      }
    }
    this.environment.dependencies.set(identity, reference);
    return reference;
  }

  resource<T>(name: string, options?: ResourceOptions<T>): Resource<T> {
    this.check();
    if (!options) return this.session.hostResource<T>(name);
    if (this.environment.resources.has(name)) refuse(Errors.Invalid.DuplicateResource, name);
    const resource = this.session.resources.declare(this.environment, name, options);
    this.environment.resources.set(name, resource);
    return resource.handle;
  }

  task(start: (scope: TaskScope) => Completion<void>): void {
    this.check();
    if (typeof start !== "function") refuse(Errors.Invalid.Task);
    this.environment.tasks.push({ start });
  }

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

  hook(name: string): { run(context: Context, input?: unknown): Promise<unknown> } {
    this.check();
    if (!this.session.kernel.features.hooks) refuse(Errors.Invalid.HooksDisabled);
    if (!this.environment.facet.definition.hooks?.[name]) refuse(Errors.Invalid.Hook, name);
    return {
      run: (context, input) => this.session.runHook(this.environment, name, context, input),
    };
  }
}

export function consumerState<T>(
  binding: StateBinding<T>,
  check: () => void,
  cleanups: Set<() => void>,
): Readable<T> {
  return {
    get value() {
      check();
      return binding.value;
    },
    select(compute) {
      check();
      return binding.select(compute, check, cleanups);
    },
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
