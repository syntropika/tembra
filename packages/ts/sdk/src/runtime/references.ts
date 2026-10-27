import { consumerState } from "../authoring/declaration.ts";
import { serviceAcquisition } from "../authoring/services.ts";
import type {
  AcquisitionOptions,
  DynamicReference,
  ServiceDefinition,
} from "../authoring/types.ts";
import { Code, Status } from "../bridge/identifiers.ts";
import type { Acquisition, KernelOrb } from "../bridge/kernel.ts";
import { KernelOp } from "../bridge/protocol.ts";
import { refuse, TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import { Context, context, isContext } from "./context.ts";
import type { Address, OrbRecord, ObjectRecord, ReferenceRecord } from "./records.ts";
import { deferred } from "./records.ts";
import type { Session } from "./session.ts";

const references = new WeakMap<object, { session: Session; record: ReferenceRecord }>();
const finalizer =
  typeof FinalizationRegistry === "undefined"
    ? undefined
    : new FinalizationRegistry<{ session: WeakRef<Session>; id: number }>((held) =>
        held.session.deref()?.handles.release(held.id),
      );

/** JS reference handles and per-object visibility. */
export class References {
  /** Retain handle metadata and index references by object. */
  constructor(readonly session: Session) {}

  #metadata = new Map<number, KernelOrb>();
  #byObject = new Map<number, Set<ReferenceRecord>>();

  /** Activate an object reserved by the native facade in this shared kernel. */
  activateAcquired(
    acquired: Acquisition,
    address: Address,
    requiredMethods: string[] = [],
  ): Promise<void> {
    const session = this.session;
    let object = session.objects.get(acquired.object);
    if (!object) {
      const activation = deferred<void>();
      object = {
        id: acquired.object,
        address,
        orb: acquired.orb,
        versions: new Map(),
        activation: activation.promise,
      };
      session.objects.set(object.id, object);
      const record = object;
      session.track(
        session.scopes.activate(record, acquired).then(activation.resolve, (error) => {
          record.error = error;
          try {
            session.kernel.command(KernelOp.Fail, { object: record.id });
          } catch {
            // Concurrent shutdown can already have retired the reserved object.
          }
          activation.reject(error);
          session.lifecycle.flush();
        }),
      );
    }
    // Every native claim needs its own policy and reload index, including reused objects.
    if (!this.claimExists(acquired.reference)) return object.activation;
    const provider = this.orbMetadata(object.orb);
    const methods =
      provider.services.find((service) => service.name === address.service)?.methods ?? [];
    const record: ReferenceRecord = {
      id: acquired.reference,
      object: acquired.object,
      address,
      methods: new Set(methods),
      requiredMethods: new Set(requiredMethods),
      stateCleanups: new Set(),
      ready: object.activation,
      released: false,
      nativeOwned: true,
    };
    const access = session.orbs.records.get(object.orb)?.definition.services?.[address.service]
      ?.access;
    try {
      this.referenceMethods(record, methods, access);
    } catch (error) {
      if (session.kernel.features.views && this.claimExists(record.id))
        session.kernel.command(KernelOp.Restrict, { reference: record.id, methods: [] });
      throw error;
    }
    session.references.set(record.id, record);
    let owned = this.#byObject.get(record.object);
    if (!owned) this.#byObject.set(record.object, (owned = new Set()));
    owned.add(record);
    return object.activation;
  }

  /** Acquire ownership and create a local service handle. */
  hold(
    definition: string | ServiceDefinition,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
    ctx?: Context,
  ): DynamicReference {
    let service: string;
    if (typeof definition === "string") service = definition;
    else {
      const acquisition = serviceAcquisition(definition, options);
      service = acquisition.name;
      options = acquisition.options;
    }
    const session = this.session;
    if (ctx) context(ctx, session);
    session.kernel.command(KernelOp.Tick, { now: session.now() });
    const address: Address = {
      service,
      key: typeof key === "string" ? [key] : [...key],
      principal: options.principal ?? ctx?.principal ?? session.principal,
    };
    const acquired = session.kernel.command<Acquisition>(KernelOp.Acquire, {
      ...address,
      apiVersion: options.apiVersion ?? 0,
      methods: options.methods ?? [],
    });
    let object = session.objects.get(acquired.object);
    if (!object) {
      const activation = deferred<void>();
      object = {
        id: acquired.object,
        address,
        orb: acquired.orb,
        versions: new Map(),
        activation: activation.promise,
      };
      session.objects.set(object.id, object);
      const record = object;
      queueMicrotask(() => {
        session.track(
          session.scopes.activate(record, acquired).then(activation.resolve, (error) => {
            record.error = error;
            try {
              session.kernel.command(KernelOp.Fail, { object: record.id });
            } catch {
              /* A concurrent close may already retire activation. */
            }
            activation.reject(error);
            session.lifecycle.flush();
          }),
        );
      });
    }
    const provider = this.orbMetadata(acquired.orb);
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
    const nativeAccess = session.options.native?.access?.(acquired.orb, service, address.principal);
    const access = nativeAccess
      ? () => ({ methods: nativeAccess })
      : session.orbs.records.get(acquired.orb)?.definition.services?.[service]?.access;
    try {
      this.referenceMethods(record, methods, access);
    } catch (error) {
      session.kernel.command(KernelOp.Release, { reference: record.id });
      session.kernel.command(KernelOp.Forget, { reference: record.id });
      session.lifecycle.flush();
      throw error;
    }
    session.references.set(record.id, record);
    let owned = this.#byObject.get(record.object);
    if (!owned) {
      owned = new Set();
      this.#byObject.set(record.object, owned);
    }
    owned.add(record);
    if (ctx) {
      let owned = session.rootReferences.get(ctx.root);
      if (!owned) {
        owned = new Set();
        session.rootReferences.set(ctx.root, owned);
      }
      owned.add(record.id);
    }
    const check = () => {
      if (record.released) throw new TembraError(Errors.Released.Reference);
      if (session.closing) throw new TembraError(Errors.Gone.Reference);
      const object = session.objects.get(record.object);
      if (!object || object.disposing || object.error) throw new TembraError(Errors.Gone.Reference);
      if (!object.live) throw new TembraError(Errors.Unavailable.NotReady);
    };
    const target = {
      /** Wait for the acquired object's physical activation. */
      get ready() {
        return record.ready;
      },
      /** Read current kernel status unless this handle is already invalid. */
      get status() {
        return record.released
          ? Status.Released
          : recordSession.closing
            ? Status.Gone
            : recordSession.kernel.command(KernelOp.Status, { reference: record.id });
      },
      /** Expose a state view bound to this reference's lifetime. */
      get state() {
        check();
        const binding = recordSession.objects.get(record.object)?.binding;
        if (!binding) refuse(Errors.Invalid.State, "Service does not expose state");
        return consumerState(binding, check, record.stateCleanups);
      },
      /** Release this handle's claim and detach its state observers. */
      release: () => this.release(record.id),
      /** Read only resources explicitly exposed by the local service. */
      opaque: (name: string) => {
        check();
        const object = session.objects.get(record.object)!;
        const resource = object.versions.get(object.orb)?.resources.get(name);
        if (!resource?.options.opaque) throw new TembraError(Errors.Forbidden.Opaque, name);
        return resource.handle.value;
      },
    };
    const recordSession = session;
    const proxy = new Proxy(target, {
      /** Resolve service methods without exposing inherited object members. */
      get: (target, property, receiver) => {
        if (property === "then") return undefined;
        if (Object.hasOwn(target, property)) return Reflect.get(target, property, receiver);
        if (typeof property !== "string") return undefined;
        if (!record.methods.has(property)) return undefined;
        return (...arguments_: unknown[]) => {
          let callContext = record.context;
          if (isContext(arguments_.at(-1))) callContext = arguments_.pop() as Context;
          return callContext
            ? session.calls.invoke(record, property, arguments_, context(callContext, session))
            : session.calls.root(
                (ctx) => session.calls.invoke(record, property, arguments_, ctx),
                false,
                undefined,
                record.address.principal,
              );
        };
      },
    }) as DynamicReference;
    references.set(proxy, { session: session, record });
    finalizer?.register(proxy, { session: new WeakRef(session), id: record.id }, record);
    return proxy;
  }

  /** Release the acquired claim if activation fails. */
  async get(
    service: string | ServiceDefinition,
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

  /** Require a reference issued by this session. */
  reference(value: unknown): ReferenceRecord {
    const issued = typeof value === "object" && value !== null ? references.get(value) : undefined;
    if (!issued || issued.session !== this.session)
      refuse(Errors.Invalid.Reference, "An issued local reference is required");
    return issued.record;
  }

  /** Cache native orb metadata once per version. */
  private orbMetadata(id: number): KernelOrb {
    const definition = this.session.orbs.records.get(id)?.info;
    if (definition) return definition;
    let metadata = this.#metadata.get(id);
    if (!metadata) {
      metadata = this.session.kernel.command<KernelOrb>(KernelOp.Orb, { id });
      this.#metadata.set(id, metadata);
    }
    return metadata;
  }

  /** Intersect declared requirements with current access permissions. */
  private referenceMethods(
    record: ReferenceRecord,
    methods: Iterable<string>,
    access?: ServiceDefinition["access"],
    resetAccess = false,
  ): void {
    const session = this.session;
    if (access && !session.kernel.features.views) throw new TembraError(Errors.Unsupported.Views);
    const allowed = access ? new Set(access(record.address.principal).methods) : undefined;
    const visible = new Set(
      [...methods].filter(
        (method) =>
          (!record.requiredMethods.size || record.requiredMethods.has(method)) &&
          (!allowed || allowed.has(method)),
      ),
    );
    if (access || (resetAccess && session.kernel.features.views))
      session.kernel.command(KernelOp.Restrict, {
        reference: record.id,
        methods: [...visible],
      });
    record.methods = visible;
  }

  /** Update only references indexed under the changed object. */
  refreshReferences(object: ObjectRecord, orb: OrbRecord): void {
    const session = this.session;
    const service = orb.definition.services?.[object.address.service];
    if (!service) return;
    const methods = Object.keys(service.methods);
    for (const reference of this.#byObject.get(object.id) ?? []) {
      if (reference.released) continue;
      if (reference.nativeOwned && !this.claimExists(reference.id)) {
        this.revoke(reference.id);
        continue;
      }
      try {
        this.referenceMethods(reference, methods, service.access, true);
      } catch (error) {
        reference.methods.clear();
        if (!reference.released && session.kernel.features.views)
          session.kernel.command(KernelOp.Restrict, { reference: reference.id, methods: [] });
        session.report(error);
      }
    }
  }

  /** Native claim release can precede its asynchronous metadata notification. */
  private claimExists(reference: number): boolean {
    try {
      const status = this.session.kernel.command<Status>(KernelOp.Status, { reference });
      return status !== Status.Released && status !== Status.Gone;
    } catch (error) {
      if (
        error instanceof TembraError &&
        (error.code === Code.Gone || error.code === Code.Released)
      )
        return false;
      throw error;
    }
  }

  /** Detach state observers and remove local reference indexes. */
  private forget(record: ReferenceRecord): void {
    for (const cleanup of record.stateCleanups) cleanup();
    record.stateCleanups.clear();
    this.session.references.delete(record.id);
    const owned = this.#byObject.get(record.object);
    owned?.delete(record);
    if (!owned?.size) this.#byObject.delete(record.object);
  }

  /** Release the kernel claim and local observers once. */
  release(id: number): void {
    const session = this.session;
    const record = session.references.get(id);
    if (!record || record.released) return;
    record.released = true;
    finalizer?.unregister(record);
    if (session.lifecycle.closedDiagnostics) {
      this.forget(record);
      return;
    }
    session.kernel.command(KernelOp.Tick, { now: session.now() });
    session.kernel.command(KernelOp.Release, { reference: id });
    session.kernel.command(KernelOp.Forget, { reference: id });
    this.forget(record);
    session.lifecycle.flush();
  }

  /** Revokes a JS handle after the engine has released its claim. */
  revoke(id: number): void {
    const record = this.session.references.get(id);
    if (!record) return;
    record.released = true;
    finalizer?.unregister(record);
    this.forget(record);
  }

  /** Remove cached metadata for a retired orb version. */
  retire(orb: number): void {
    this.#metadata.delete(orb);
  }

  /** Detach all remaining observers during host shutdown. */
  clear(): void {
    for (const reference of this.session.references.values()) this.forget(reference);
    this.session.references.clear();
    this.#byObject.clear();
    this.#metadata.clear();
  }
}
