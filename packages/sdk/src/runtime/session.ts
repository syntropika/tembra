import { Errors } from "../errors.ts";
import { ActionType, EnvironmentPhase, KernelOp, Outcome, OwnerKind } from "../protocol.ts";
import { Code, Reason, Status } from "../identifiers.ts";
import { failure, refuse, TembraError } from "../error.ts";
import { jsonCodec } from "../codec.ts";
import { Telemetry } from "../telemetry.ts";
import { Kernel } from "../kernel.ts";
import type {
  Acquisition,
  Action,
  Diagnostics,
  Invocation,
  KernelFacet,
  Owner,
  Snapshot,
} from "../kernel.ts";
import { Values } from "../values.ts";
import { StateBinding } from "../state.ts";
import type { StateBackend } from "../state.ts";
import type {
  AcquisitionOptions,
  Completion,
  DynamicReference,
  HostOptions,
  Resource,
  ResourceOptions,
  ServiceDefinition,
} from "../types.ts";
import { Context, context, isContext } from "./context.ts";
import { Declaration, consumerState } from "./declaration.ts";
import { Resources } from "./resources.ts";
import { Activities } from "./activities.ts";
import { Facets } from "./facets.ts";
import { Replacements } from "./reload.ts";
import { Programs } from "./programs.ts";
import { deferred } from "./records.ts";
import type {
  Address,
  Environment,
  FacetRecord,
  ObjectRecord,
  ReferenceRecord,
  ResourceRecord,
} from "./records.ts";

const references = new WeakMap<object, { session: Session; record: ReferenceRecord }>();
const finalizer =
  typeof FinalizationRegistry === "undefined"
    ? undefined
    : new FinalizationRegistry<{ session: WeakRef<Session>; id: number }>((held) =>
        held.session.deref()?.release(held.id),
      );

/** The JS adapter owns JS values and execution; kernel metadata remains authoritative. */
export class Session implements StateBackend {
  readonly kernel: Kernel;
  readonly values: Values;
  readonly resources = new Resources(this);
  readonly activities = new Activities(this);
  readonly facets = new Facets(this);
  readonly replacements = new Replacements(this);
  readonly programs = new Programs(this);
  readonly objects = new Map<number, ObjectRecord>();
  readonly references = new Map<number, ReferenceRecord>();
  readonly rootReferences = new Map<number, Set<number>>();
  readonly controllers = new Map<number, AbortController>();
  readonly jobs = new Set<Promise<unknown>>();
  readonly cleanupErrors: unknown[] = [];
  hostScope: Environment;
  readonly codec;
  readonly name: string;
  readonly principal: string;
  readonly errors: TembraError[] = [];
  readonly now: () => number;
  readonly telemetry: Telemetry | undefined;
  closing = false;
  #closed?: Promise<void>;
  #flushing = false;
  #publishing = false;
  #publications: (() => void)[] = [];
  #linger: ReturnType<typeof setTimeout> | undefined;
  #lingerDeadline: number | undefined;
  #finalDiagnostics: ReturnType<Session["diagnostics"]> | undefined;
  #facetMetadata = new Map<number, KernelFacet>();
  #objectReferences = new Map<number, Set<ReferenceRecord>>();
  #hostResources = new Map<string, ResourceRecord>();
  #pendingHostScopes = new Set<Environment>();
  #hostActivation: Promise<void> | undefined;

  constructor(readonly options: HostOptions) {
    this.kernel = new Kernel(options.kernel);
    if (options.telemetry && !this.kernel.features.telemetry)
      throw new TembraError(Errors.Unsupported.Telemetry);
    this.telemetry = options.telemetry
      ? new Telemetry(options.telemetry.capacity, options.telemetry.sampleEvery)
      : undefined;
    this.values = new Values(() => this.kernel.command<number>(KernelOp.Allocate));
    this.codec = options.serialization ?? jsonCodec;
    this.name = options.name ?? "host";
    this.principal = options.principal ?? "host";
    const clock = options.now ?? (() => performance.now());
    const origin = clock();
    this.now = () => Math.floor(clock() - origin);
    const hostFacet: FacetRecord = {
      info: {
        id: 0,
        owner: 0,
        name: "$host",
        apiVersion: 1,
        codeVersion: 1,
        services: [],
        hooks: [],
        requires: [],
      },
      definition: { name: "$host", apiVersion: 1 },
    };
    this.hostScope = this.environment({ kind: OwnerKind.Host }, hostFacet);
    for (const [name, declaration] of Object.entries(options.resources ?? {})) {
      new Declaration(this, this.hostScope).resource(name, declaration);
    }
    for (const [name, resource] of this.hostScope.resources)
      this.#hostResources.set(name, resource);
    this.hostScope.phase = EnvironmentPhase.Prepared;
    this.programs.register(this.hostScope);
  }

  environment(owner: Owner, facet: FacetRecord, object?: ObjectRecord): Environment {
    const environment: Environment = {
      id: this.kernel.command<number>(KernelOp.Allocate),
      owner,
      facet,
      ...(object ? { object } : {}),
      phase: EnvironmentPhase.Declaring,
      value: {},
      dependencies: new Map(),
      resources: new Map(),
      tasks: [],
      listeners: [],
    };
    this.programs.scopes.set(environment.id, environment);
    return environment;
  }

  principalFor(object: number): string {
    return this.objects.get(object)?.address.principal ?? this.principal;
  }
  snapshot(object: number): Snapshot {
    return this.kernel.command(KernelOp.Snapshot, { object });
  }
  value<T>(id: number): T {
    if (this.values.has(id)) return this.values.get<T>(id);
    if (!this.options.native) refuse(Errors.Invalid.ValueGone);
    const value = this.options.native.value(id) as T;
    this.values.set(id, value);
    return value;
  }

  check(value: unknown, object: number): Invocation {
    const ctx = context(value, this);
    if (!ctx.invocation || ctx.invocation.object !== object)
      throw new TembraError(Errors.Forbidden.StateOwner);
    return ctx.invocation;
  }

  adopt(object: number, value: unknown, revision: string, candidate: unknown): Snapshot {
    const invocation = this.check(value, object);
    const id = this.values.put(candidate);
    try {
      return this.kernel.command(KernelOp.StateAdopt, {
        call: invocation.id,
        object,
        revision,
        value: id,
      });
    } catch (error) {
      this.values.drop(id);
      throw error;
    } finally {
      this.flush();
    }
  }

  publish(operation: () => void): void {
    this.#publications.push(operation);
    if (this.#publishing) return;
    this.#publishing = true;
    try {
      while (this.#publications.length) this.#publications.shift()!();
    } finally {
      this.#publishing = false;
    }
  }

  report(error: unknown): void {
    const reported = failure(error);
    if (this.errors.length === 64) this.errors.shift();
    this.errors.push(reported);
    try {
      this.options.onError?.(reported);
    } catch {
      /* Error reporting cannot invalidate an adopted state. */
    }
  }

  track<T>(job: Promise<T>, cleanup = false): Promise<T> {
    this.jobs.add(job);
    job.then(
      () => {
        this.jobs.delete(job);
      },
      (error) => {
        this.jobs.delete(job);
        if (cleanup) {
          this.cleanupErrors.push(error);
          this.report(error);
        }
      },
    );
    return job;
  }

  signal(root: number): AbortSignal {
    let controller = this.controllers.get(root);
    if (!controller) {
      controller = new AbortController();
      this.controllers.set(root, controller);
    }
    return controller.signal;
  }

  async root<T>(
    operation: (context: Context) => Completion<T>,
    cleanup = false,
    issuedRoot?: number,
    principal = this.principal,
  ): Promise<T> {
    const root =
      issuedRoot ?? this.kernel.command<number>(cleanup ? "cleanup-root" : "root", { principal });
    const ctx = new Context(this, root);
    try {
      return await operation(ctx);
    } finally {
      for (const reference of this.rootReferences.get(root) ?? []) this.release(reference);
      this.rootReferences.delete(root);
      if (!this.#finalDiagnostics) this.kernel.command(KernelOp.EndRoot, { root });
      this.controllers.delete(root);
      this.flush();
    }
  }

  hold(
    service: string,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
    ctx?: Context,
  ): DynamicReference {
    if (ctx) context(ctx, this);
    this.kernel.command(KernelOp.Tick, { now: this.now() });
    const address: Address = {
      service,
      key: typeof key === "string" ? [key] : [...key],
      principal: options.principal ?? ctx?.principal ?? this.principal,
    };
    const acquired = this.kernel.command<Acquisition>(KernelOp.Acquire, {
      ...address,
      apiVersion: options.apiVersion ?? 0,
      methods: options.methods ?? [],
    });
    let object = this.objects.get(acquired.object);
    if (!object) {
      const activation = deferred<void>();
      object = {
        id: acquired.object,
        address,
        facet: acquired.facet,
        versions: new Map(),
        activation: activation.promise,
      };
      this.objects.set(object.id, object);
      const record = object;
      queueMicrotask(() => {
        this.track(
          this.activate(record, acquired).then(activation.resolve, (error) => {
            record.error = error;
            try {
              this.kernel.command(KernelOp.Fail, { object: record.id });
            } catch {
              /* A concurrent close may already retire activation. */
            }
            activation.reject(error);
            this.flush();
          }),
        );
      });
    }
    const provider = this.facetMetadata(acquired.facet);
    const methods = new Set(provider.services.find((s) => s.name === service)?.methods ?? []);
    const record: ReferenceRecord = {
      id: acquired.reference,
      object: acquired.object,
      address,
      methods,
      requiredMethods: new Set(options.methods ?? []),
      stateCleanups: new Set(),
      ready: object.activation,
      released: false,
      ...(ctx ? { context: ctx } : {}),
    };
    const access = this.facets.records.get(acquired.facet)?.definition.services?.[service]?.access;
    try {
      this.referenceMethods(record, methods, access);
    } catch (error) {
      this.kernel.command(KernelOp.Release, { reference: record.id });
      this.kernel.command(KernelOp.Forget, { reference: record.id });
      this.flush();
      throw error;
    }
    this.references.set(record.id, record);
    let owned = this.#objectReferences.get(record.object);
    if (!owned) {
      owned = new Set();
      this.#objectReferences.set(record.object, owned);
    }
    owned.add(record);
    if (ctx) {
      let owned = this.rootReferences.get(ctx.root);
      if (!owned) {
        owned = new Set();
        this.rootReferences.set(ctx.root, owned);
      }
      owned.add(record.id);
    }
    const check = () => {
      if (record.released) throw new TembraError(Errors.Released.Reference);
      if (this.closing) throw new TembraError(Errors.Gone.Reference);
      const object = this.objects.get(record.object);
      if (!object || object.disposing || object.error) throw new TembraError(Errors.Gone.Reference);
      if (!object.live) throw new TembraError(Errors.Unavailable.NotReady);
    };
    const target = {
      get ready() {
        return record.ready;
      },
      get status() {
        return record.released
          ? Status.Released
          : recordSession.closing
            ? Status.Gone
            : recordSession.kernel.command(KernelOp.Status, { reference: record.id });
      },
      get state() {
        check();
        const binding = recordSession.objects.get(record.object)?.binding;
        if (!binding) refuse(Errors.Invalid.State, "Service does not expose state");
        return consumerState(binding, check, record.stateCleanups);
      },
      release: () => this.release(record.id),
      opaque: (name: string) => {
        check();
        const object = this.objects.get(record.object)!;
        const resource = object.versions.get(object.facet)?.resources.get(name);
        if (!resource?.options.opaque) throw new TembraError(Errors.Forbidden.Opaque, name);
        return resource.handle.value;
      },
    };
    const recordSession = this;
    const proxy = new Proxy(target, {
      get: (target, property, receiver) => {
        if (property === "then") return undefined;
        if (Object.hasOwn(target, property)) return Reflect.get(target, property, receiver);
        if (typeof property !== "string") return undefined;
        if (!record.methods.has(property)) return undefined;
        return (...arguments_: unknown[]) => {
          let callContext = record.context;
          if (isContext(arguments_.at(-1))) callContext = arguments_.pop() as Context;
          return callContext
            ? this.invoke(record, property, arguments_, context(callContext, this))
            : this.root(
                (ctx) => this.invoke(record, property, arguments_, ctx),
                false,
                undefined,
                record.address.principal,
              );
        };
      },
    }) as DynamicReference;
    references.set(proxy, { session: this, record });
    finalizer?.register(proxy, { session: new WeakRef(this), id: record.id }, record);
    return proxy;
  }

  async get(
    service: string,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
    ctx?: Context,
  ): Promise<DynamicReference> {
    const reference = this.hold(service, key, options, ctx);
    try {
      await reference.ready;
      return reference;
    } catch (error) {
      reference.release();
      throw error;
    }
  }

  reference(value: unknown): ReferenceRecord {
    const issued = typeof value === "object" && value !== null ? references.get(value) : undefined;
    if (!issued || issued.session !== this)
      refuse(Errors.Invalid.Reference, "An issued local reference is required");
    return issued.record;
  }

  private facetMetadata(id: number): KernelFacet {
    const definition = this.facets.records.get(id)?.info;
    if (definition) return definition;
    let metadata = this.#facetMetadata.get(id);
    if (!metadata) {
      metadata = this.kernel.command<KernelFacet>(KernelOp.Facet, { id });
      this.#facetMetadata.set(id, metadata);
    }
    return metadata;
  }

  private referenceMethods(
    record: ReferenceRecord,
    methods: Iterable<string>,
    access?: ServiceDefinition["access"],
    resetAccess = false,
  ): void {
    if (access && !this.kernel.features.views) throw new TembraError(Errors.Unsupported.Views);
    const allowed = access ? new Set(access(record.address.principal).methods) : undefined;
    const visible = new Set(
      [...methods].filter(
        (method) =>
          (!record.requiredMethods.size || record.requiredMethods.has(method)) &&
          (!allowed || allowed.has(method)),
      ),
    );
    if (access || (resetAccess && this.kernel.features.views))
      this.kernel.command(KernelOp.Restrict, { reference: record.id, methods: [...visible] });
    record.methods = visible;
  }

  refreshReferences(object: ObjectRecord, facet: FacetRecord): void {
    const service = facet.definition.services?.[object.address.service];
    if (!service) return;
    const methods = Object.keys(service.methods);
    for (const reference of this.#objectReferences.get(object.id) ?? []) {
      if (reference.released) continue;
      try {
        this.referenceMethods(reference, methods, service.access, true);
      } catch (error) {
        reference.methods.clear();
        if (!reference.released && this.kernel.features.views)
          this.kernel.command(KernelOp.Restrict, { reference: reference.id, methods: [] });
        this.report(error);
      }
    }
  }

  private forgetReference(record: ReferenceRecord): void {
    for (const cleanup of record.stateCleanups) cleanup();
    record.stateCleanups.clear();
    this.references.delete(record.id);
    const owned = this.#objectReferences.get(record.object);
    owned?.delete(record);
    if (!owned?.size) this.#objectReferences.delete(record.object);
  }

  release(id: number): void {
    const record = this.references.get(id);
    if (!record || record.released) return;
    record.released = true;
    finalizer?.unregister(record);
    if (this.#finalDiagnostics) {
      this.forgetReference(record);
      return;
    }
    this.kernel.command(KernelOp.Tick, { now: this.now() });
    this.kernel.command(KernelOp.Release, { reference: id });
    this.kernel.command(KernelOp.Forget, { reference: id });
    this.forgetReference(record);
    this.flush();
    this.scheduleLinger();
  }

  private async activate(object: ObjectRecord, acquired: Acquisition): Promise<void> {
    if (this.options.native?.hasFacet(acquired.facet)) {
      await this.options.native.activate(
        object.id,
        acquired.facet,
        object.address.service,
        object.address.key,
        object.address.principal,
      );
      const scopes = this.kernel.command<number[]>(KernelOp.ScopesForOwner, {
        owner: { kind: OwnerKind.Object, id: object.id },
      });
      await Promise.all(
        scopes.map((scope) =>
          this.programs.run(this.kernel.command<number>(KernelOp.ScopeActivate, { scope })),
        ),
      );
      try {
        this.snapshot(object.id);
        object.binding = new StateBinding(this, object.id);
      } catch {
        /* State is optional. */
      }
    } else {
      const facet = this.facets.records.get(acquired.facet);
      if (!facet) refuse(Errors.Invalid.FacetGone);
      await this.facets.ready(facet);
      const environment = this.createEnvironment(object, facet);
      object.versions.set(facet.info.id, environment);
      await this.openEnvironment(environment);
    }
    if (
      this.kernel.command<Status>(KernelOp.Status, { reference: acquired.reference }) ===
      Status.Loading
    )
      this.kernel.command(KernelOp.Ready, { object: object.id });
    object.live = true;
    this.flush();
  }

  createEnvironment(object: ObjectRecord, facet: FacetRecord): Environment {
    const definition = facet.definition.services?.[object.address.service];
    if (!definition) refuse(Errors.Invalid.ServiceGone);
    const environment = this.environment({ kind: OwnerKind.Object, id: object.id }, facet, object);
    try {
      const value = definition.create?.(new Declaration(this, environment)) ?? {};
      if (value && typeof (value as { then?: unknown }).then === "function")
        refuse(Errors.Invalid.AsyncFactory, "Factories must be synchronous");
      environment.value = value;
      return environment;
    } finally {
      environment.phase = EnvironmentPhase.Prepared;
      this.programs.register(environment);
    }
  }

  async openEnvironment(environment: Environment): Promise<void> {
    await this.programs.activate(environment);
  }

  readyHostScope(): Promise<void> {
    if (this.closing) return Promise.reject(new TembraError(Errors.Closed.Host));
    if (this.#hostActivation) return this.#hostActivation;
    const activation = (async () => {
      if (this.hostScope.phase === EnvironmentPhase.Closed)
        this.hostScope = await this.redeclareHostScope(this.hostScope);
      await this.openEnvironment(this.hostScope);
      while (this.#pendingHostScopes.size) {
        let environment = this.#pendingHostScopes.values().next().value!;
        if (
          environment.phase === EnvironmentPhase.Closed ||
          [...environment.resources.values()].some((resource) =>
            this.resources
              .dependencyIds(resource.options)
              .some((id, index) => id !== resource.dependencies[index]),
          )
        ) {
          const previous = environment;
          environment = await this.redeclareHostScope(previous);
          this.#pendingHostScopes.delete(previous);
          this.#pendingHostScopes.add(environment);
        }
        await this.openEnvironment(environment);
        this.#pendingHostScopes.delete(environment);
      }
    })();
    this.#hostActivation = activation;
    const clear = () => {
      if (this.#hostActivation === activation) this.#hostActivation = undefined;
    };
    activation.then(clear, clear);
    return activation;
  }

  private async redeclareHostScope(previous: Environment): Promise<Environment> {
    await this.programs.cleanup([previous]);
    const environment = this.environment({ kind: OwnerKind.Host }, previous.facet);
    this.resources.redeclare(environment, previous);
    environment.phase = EnvironmentPhase.Prepared;
    this.programs.register(environment);
    return environment;
  }

  async invoke(
    reference: ReferenceRecord,
    method: string,
    arguments_: unknown[],
    ctx: Context,
  ): Promise<unknown> {
    context(ctx, this);
    if (reference.released) throw new TembraError(Errors.Released.Reference);
    await reference.ready;
    ctx.check();
    const invocation = this.kernel.command<Invocation>(KernelOp.BeginCall, {
      reference: reference.id,
      root: ctx.root,
      method,
    });
    const entered = new Context(this, ctx.root, invocation);
    const physical = this.track(
      (async () => {
        const sampled = this.telemetry?.sample();
        const started = sampled ? this.now() : 0;
        let outcome: Outcome = Outcome.Ok;
        try {
          if (this.options.native?.hasFacet(invocation.facet))
            return await this.options.native.invoke(invocation, arguments_, entered);
          const object = this.objects.get(invocation.object);
          const environment = object?.versions.get(invocation.facet);
          const handle =
            environment?.facet.definition.services?.[reference.address.service]?.methods[method];
          if (!environment || !handle) throw new TembraError(Errors.Gone.Implementation);
          return await handle(environment.value, entered, ...arguments_);
        } catch (error) {
          outcome = Outcome.Error;
          throw error;
        } finally {
          if (sampled)
            this.telemetry!.record({
              service: reference.address.service,
              method,
              root: invocation.root,
              codeVersion: invocation.codeVersion,
              duration: this.now() - started,
              outcome,
            });
          this.kernel.command(KernelOp.Tick, { now: this.now() });
          this.kernel.command(KernelOp.FinishCall, { call: invocation.id });
          this.flush();
          this.scheduleLinger();
        }
      })(),
    );
    return this.observe(physical, ctx.signal);
  }

  /** Fresh callback roots inherit owner identity; supplied roots retain the caller's authority. */
  async owned<T>(
    environment: Environment,
    operation: number,
    handle: (ctx: Context) => Completion<T>,
    root?: number,
  ): Promise<T> {
    const execute = async (ctx: Context) => {
      const invocation = this.kernel.command<Invocation>(KernelOp.BeginOperation, {
        facet: environment.facet.info.id,
        root: ctx.root,
        operation,
        ...(environment.object ? { object: environment.object.id } : {}),
      });
      try {
        return await handle(new Context(this, ctx.root, invocation));
      } finally {
        this.kernel.command(KernelOp.FinishCall, { call: invocation.id });
        this.flush();
      }
    };
    return root === undefined
      ? this.root(execute, false, undefined, this.principalFor(environment.object?.id ?? 0))
      : execute(new Context(this, root));
  }

  async observe<T>(physical: Promise<T>, signal: AbortSignal): Promise<T> {
    if (signal.aborted) throw signal.reason;
    let abort!: () => void;
    const cancelled = new Promise<never>((_, reject) => {
      abort = () => reject(signal.reason ?? new TembraError(Errors.Cancelled.Root));
      signal.addEventListener("abort", abort, { once: true });
    });
    try {
      return await Promise.race([physical, cancelled]);
    } finally {
      signal.removeEventListener("abort", abort);
    }
  }

  requirement(
    environment: Environment,
    service: string,
  ): { service: string; apiVersion?: number; methods?: readonly string[] } {
    const own = environment.facet.definition.services?.[service];
    if (own) return { service, apiVersion: environment.facet.info.apiVersion };
    for (const [alias, requirement] of Object.entries(
      environment.facet.definition.requires ?? {},
    )) {
      let provider: KernelFacet;
      try {
        provider = this.kernel.command<KernelFacet>(KernelOp.FacetNamed, {
          name: requirement.facet ?? alias,
          ticket: environment.facet.ticket,
        });
      } catch (error) {
        if (
          error instanceof TembraError &&
          error.code === Code.Gone &&
          error.reason === Reason.Facet
        )
          continue;
        throw error;
      }
      const selected =
        alias === service
          ? (provider.services.find((s) => s.name === alias) ??
            (provider.services.length === 1 ? provider.services[0] : undefined))
          : provider.services.find((s) => s.name === service);
      if (!selected) continue;
      const methods = requirement.services?.[selected.name] ?? requirement.methods;
      if (!methods) throw new TembraError(Errors.Forbidden.UndeclaredService, service);
      return { service: selected.name, apiVersion: requirement.apiVersion, methods };
    }
    throw new TembraError(Errors.Forbidden.UndeclaredDependency, service);
  }

  hostResource<T>(name: string): Resource<T> {
    const resource = this.#hostResources.get(name);
    if (!resource) throw new TembraError(Errors.Forbidden.HostResource, name);
    return resource.handle;
  }

  resource<T>(name: string, options: ResourceOptions<T>): Resource<T> {
    if (this.closing) throw new TembraError(Errors.Closed.Host);
    if (this.#hostResources.has(name)) refuse(Errors.Invalid.DuplicateResource);
    const environment = this.environment({ kind: OwnerKind.Host }, this.hostScope.facet);
    try {
      const resource = new Declaration(this, environment).resource(name, options);
      this.#hostResources.set(name, environment.resources.get(name)!);
      environment.phase = EnvironmentPhase.Prepared;
      this.programs.register(environment);
      this.#pendingHostScopes.add(environment);
      this.track(this.readyHostScope()).catch((error) => this.report(error));
      return resource;
    } catch (error) {
      this.programs.scopes.delete(environment.id);
      throw error;
    }
  }

  runHook(environment: Environment, name: string, ctx: Context, input: unknown): Promise<unknown> {
    return this.facets.runHook(environment, name, context(ctx, this), input);
  }

  flush(): void {
    if (this.#flushing) return;
    this.#flushing = true;
    try {
      for (;;) {
        const actions = this.kernel.command<Action[]>(KernelOp.Drain);
        if (!actions.length) break;
        for (const action of actions) {
          try {
            this.action(action);
          } catch (error) {
            this.report(error);
          }
        }
      }
    } finally {
      this.#flushing = false;
      this.scheduleLinger();
    }
  }

  private action(action: Action): void {
    switch (action.type) {
      case ActionType.Execute:
      case ActionType.CancelEffects:
      case ActionType.ProgramPrepared:
      case ActionType.ProgramComplete:
        this.programs.dispatch(action);
        break;
      case ActionType.DropValue:
        this.values.drop(action.id);
        this.options.native?.dropValue?.(action.id);
        break;
      case ActionType.CancelRoot:
        this.controllers.get(action.id)?.abort(new TembraError(Errors.Cancelled.Root));
        break;
      case ActionType.CancelTask:
        this.activities.cancel(action.id);
        break;
      case ActionType.Deliver:
        this.activities.deliver(action.id, action.payload!, action.operation!);
        break;
      case ActionType.DetachSubscription:
        this.activities.detach(action.id);
        break;
      case ActionType.SubscriptionFailed:
        this.activities.failed(action.id);
        break;
      case ActionType.Dispose:
        this.track(this.dispose(action.id), true);
        break;
      case ActionType.CloseResource: {
        // Resource callbacks execute only in response to the engine's ordered program effect.
        break;
      }
      case ActionType.RetireFacet:
        this.#facetMetadata.delete(action.id);
        this.facets.retire(action.id);
        break;
      case ActionType.State: {
        const object = this.objects.get(action.id);
        if (!object?.binding) break;
        const snapshot = this.snapshot(action.id);
        // Local updates already staged their selectors. Foreign Rust updates enter JS only here.
        if (!this.values.has(snapshot.value)) {
          const candidate = this.value<any>(snapshot.value);
          this.publish(object.binding.adopt(object.binding.prepare(candidate), candidate));
        }
        break;
      }
    }
  }

  private async dispose(id: number): Promise<void> {
    const object = this.objects.get(id);
    if (!object) return;
    object.disposing ??= (async () => {
      const errors: unknown[] = [];
      await object.activation.catch(() => {});
      const scopes = this.kernel.command<number[]>(KernelOp.ScopesForOwner, {
        owner: { kind: OwnerKind.Object, id },
      });
      try {
        if (scopes.length)
          await this.programs.run(this.kernel.command<number>(KernelOp.ScopesCleanup, { scopes }));
      } catch (error) {
        errors.push(error);
      }
      try {
        if (this.options.native?.hasFacet(object.facet)) await this.options.native.dispose(id);
      } catch (error) {
        errors.push(error);
      }
      object.binding?.close();
      this.kernel.command(KernelOp.Disposed, { object: id });
      this.objects.delete(id);
      this.flush();
      if (errors.length) throw new AggregateError(errors, "Object cleanup failed");
    })();
    return object.disposing;
  }

  async closeEnvironment(environment: Environment): Promise<void> {
    if (environment.phase !== EnvironmentPhase.Closed) await this.programs.cleanup([environment]);
  }

  /** Revokes a JS handle after the engine has released its claim. */
  revoke(id: number): void {
    const record = this.references.get(id);
    if (!record) return;
    record.released = true;
    finalizer?.unregister(record);
    this.forgetReference(record);
  }

  scheduleLinger(): void {
    if (this.closing) return;
    const deadline = this.kernel.command<number | null>(KernelOp.NextDeadline) ?? undefined;
    if (deadline === this.#lingerDeadline) return;
    if (this.#linger !== undefined) clearTimeout(this.#linger);
    this.#linger = undefined;
    this.#lingerDeadline = deadline;
    if (deadline === undefined) return;
    this.#linger = setTimeout(
      () => {
        if (this.#lingerDeadline !== deadline) return;
        this.#linger = undefined;
        this.#lingerDeadline = undefined;
        this.kernel.command(KernelOp.Tick, { now: this.now() });
        this.flush();
      },
      Math.max(0, deadline - this.now()),
    );
    this.#linger.unref?.();
  }

  diagnostics(): Diagnostics & { values: number; pendingJobs: number } {
    if (this.#finalDiagnostics) return this.#finalDiagnostics;
    return {
      ...this.kernel.command<Diagnostics>(KernelOp.Diagnostics),
      values: this.values.size,
      pendingJobs: this.jobs.size,
    };
  }

  close(): Promise<void> {
    this.#closed ??= (async () => {
      this.closing = true;
      clearTimeout(this.#linger);
      this.#linger = undefined;
      this.#lingerDeadline = undefined;
      this.kernel.command(KernelOp.Close);
      await this.replacements.cancel();
      this.flush();
      for (;;) {
        const pending = [...this.jobs];
        if (!pending.length) break;
        await Promise.allSettled(pending);
        this.flush();
      }
      this.kernel.command(KernelOp.FinishClose);
      this.values.clear();
      for (const reference of this.references.values()) this.forgetReference(reference);
      this.references.clear();
      this.#objectReferences.clear();
      this.facets.records.clear();
      this.#facetMetadata.clear();
      this.#hostResources.clear();
      this.#pendingHostScopes.clear();
      this.#finalDiagnostics = this.diagnostics();
      this.kernel.port.free?.();
      if (this.cleanupErrors.length)
        throw new AggregateError(this.cleanupErrors, "Host cleanup failed");
    })();
    return this.#closed;
  }
}
