import { EnvironmentPhase, KernelOp } from "../bridge/protocol.ts";
import { Errors } from "../errors.ts";

import type { Resource, ResourceOptions } from "../authoring/types.ts";
import { TembraError, refuse } from "../error.ts";
import type { Environment, ResourceRecord } from "./records.ts";
import type { Session } from "./session.ts";

const issued = new WeakMap<object, ResourceRecord & { session: Session }>();

export class Resources {
  readonly records = new Map<number, ResourceRecord>();
  readonly closedRecords = new Map<number, ResourceRecord>();

  /** Retain callbacks for live and closed resource identities. */
  constructor(readonly session: Session) {}

  /** Resolve a live or recently closed callback record by its kernel identity. */
  find(id: number): ResourceRecord | undefined {
    return this.records.get(id) ?? this.closedRecords.get(id);
  }

  /** Validate local dependency ownership and collect resource identities. */
  dependencyIds(options: ResourceOptions<any>): number[] {
    const dependencies: number[] = [];
    for (const dependency of options.deps ?? []) {
      const resource =
        typeof dependency === "object" && dependency !== null ? issued.get(dependency) : undefined;
      if (resource) {
        if (resource.session !== this.session)
          refuse(Errors.Invalid.ResourceHost, "Resource dependencies must belong to the same host");
        dependencies.push(resource.id);
      } else {
        this.session.handles.reference(dependency);
      }
    }
    return dependencies;
  }

  /** Ask Rust to resolve this named resource declaration. */
  private register(environment: Environment, name: string, options: ResourceOptions<any>): number {
    return this.session.kernel.command<{ id: number; value: number | null }>(KernelOp.Resource, {
      owner: environment.owner,
      name,
      version: options.version ?? 1,
      exclusive: options.exclusive ?? false,
      dependencies: this.dependencyIds(options),
    }).id;
  }

  /** Rebind closed host resources to a fresh declared scope. */
  redeclare(environment: Environment, previous: Environment): void {
    for (const resource of previous.resources.values()) {
      this.records.delete(resource.id);
      this.closedRecords.delete(resource.id);
      resource.reset(this.register(environment, resource.name, resource.options));
      resource.dependencies = this.dependencyIds(resource.options);
      this.records.set(resource.id, resource);
      environment.resources.set(resource.name, resource);
    }
  }

  /** Issue a local resource handle and retain its physical callbacks. */
  declare<T>(environment: Environment, name: string, options: ResourceOptions<T>): ResourceRecord {
    if (!name || typeof options.open !== "function") refuse(Errors.Invalid.Resource);
    const id = this.register(environment, name, options);
    const reused = this.records.get(id);
    if (reused) {
      // The engine's adoption effect publishes candidates; abort leaves live callbacks intact.
      (environment.resourceDeclarations ??= new Map()).set(reused, options);
      return reused;
    }
    const controller = new AbortController();
    const handle: Resource<T> = Object.freeze({
      /** Read the opened resource value after declaration has finished. */
      get value() {
        if (environment.phase === EnvironmentPhase.Declaring)
          refuse(Errors.Invalid.ResourceInFactory, "Resources open after declaration");
        const value = record.session.kernel.command<number>(KernelOp.ResourceValue, {
          resource: record.id,
        });
        return record.session.value<T>(value);
      },
      version: options.version ?? 1,
    });
    const record = {
      id,
      name,
      options,
      handle,
      dependencies: this.dependencyIds(options),
      controller,
      session: this.session,
      /** Clear physical completion state before reopening a restored resource. */
      reset: (id: number) => {
        record.id = id;
        record.controller = new AbortController();
        delete record.value;
        delete record.closed;
        delete record.started;
      },
      /** Share one opener promise until its physical callback settles. */
      start: () => {
        record.started ??= (async () => {
          const declaration = record.options;
          const value = await declaration.open(record.controller.signal);
          const id = this.session.values.put(value);
          try {
            this.session.kernel.command(KernelOp.ResourceOpened, {
              resource: record.id,
              value: id,
            });
          } catch (error) {
            this.session.values.drop(id);
            if (declaration.close)
              await this.session.calls.root((ctx) => declaration.close!(value, ctx), true);
            throw error;
          }
          record.value = id;
        })();
        record.started.catch(() => {});
        return record.started;
      },
    } as ResourceRecord & { session: Session };
    issued.set(handle, record);
    this.records.set(id, record);
    return record;
  }

  /** Wait for opening and closing callbacks before acknowledging release. */
  async close(record: ResourceRecord): Promise<void> {
    record.closed ??= (async () => {
      record.controller.abort(new TembraError(Errors.Cancelled.Resource));
      await record.started?.catch(() => {});
      try {
        if (record.value !== undefined && record.options.close) {
          const value = this.session.value(record.value);
          await this.session.calls.root((ctx) => record.options.close!(value, ctx), true);
        }
      } finally {
        this.session.kernel.command(KernelOp.ResourceClosed, { resource: record.id });
        this.records.delete(record.id);
        this.closedRecords.set(record.id, record);
        this.session.lifecycle.flush();
      }
    })();
    return record.closed;
  }

  /** Rebind a restored resource and wait for its new value. */
  async reopen(record: ResourceRecord, id: number): Promise<void> {
    this.closedRecords.delete(record.id);
    record.reset(id);
    this.records.set(id, record);
    await record.start();
  }
}
