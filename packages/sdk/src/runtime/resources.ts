import { Errors } from "../errors.ts";
import { EnvironmentPhase, KernelOp } from "../protocol.ts";

import { TembraError, refuse } from "../error.ts";
import type { Resource, ResourceOptions } from "../types.ts";
import type { Environment, ResourceRecord } from "./records.ts";
import { deferred } from "./records.ts";
import type { Session } from "./session.ts";

const issued = new WeakMap<object, ResourceRecord & { session: Session }>();

export class Resources {
  readonly records = new Map<number, ResourceRecord>();
  readonly closedRecords = new Map<number, ResourceRecord>();
  constructor(readonly session: Session) {}

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
        this.session.reference(dependency);
      }
    }
    return dependencies;
  }

  private register(environment: Environment, name: string, options: ResourceOptions<any>): number {
    return this.session.kernel.command<{ id: number; value: number | null }>(KernelOp.Resource, {
      owner: environment.owner,
      name,
      version: options.version ?? 1,
      exclusive: options.exclusive ?? false,
      dependencies: this.dependencyIds(options),
    }).id;
  }

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

  declare<T>(environment: Environment, name: string, options: ResourceOptions<T>): ResourceRecord {
    if (!name || typeof options.open !== "function") refuse(Errors.Invalid.Resource);
    const id = this.register(environment, name, options);
    const reused = this.records.get(id);
    if (reused) return reused;
    let ready = deferred<void>();
    const controller = new AbortController();
    const handle: Resource<T> = Object.freeze({
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
      owner: environment.owner,
      options,
      handle,
      dependencies: this.dependencyIds(options),
      ready: ready.promise,
      controller,
      exclusive: options.exclusive ?? false,
      version: options.version ?? 1,
      session: this.session,
      reset: (id: number) => {
        record.id = id;
        record.controller = new AbortController();
        ready = deferred<void>();
        record.ready = ready.promise;
        delete record.value;
        delete record.closed;
        delete record.started;
      },
      start: () => {
        record.started ??= (async () => {
          try {
            const value = await options.open(record.controller.signal);
            const id = this.session.values.put(value);
            try {
              this.session.kernel.command(KernelOp.ResourceOpened, {
                resource: record.id,
                value: id,
              });
            } catch (error) {
              this.session.values.drop(id);
              if (options.close) await this.session.root((ctx) => options.close!(value, ctx), true);
              throw error;
            }
            record.value = id;
            ready.resolve();
          } catch (error) {
            ready.reject(error);
            throw error;
          }
        })();
        record.started.catch(() => {});
        return record.started;
      },
    } as ResourceRecord & { session: Session };
    issued.set(handle, record);
    this.records.set(id, record);
    return record;
  }

  async close(record: ResourceRecord): Promise<void> {
    record.closed ??= (async () => {
      record.controller.abort(new TembraError(Errors.Cancelled.Resource));
      await record.started?.catch(() => {});
      try {
        if (record.value !== undefined && record.options.close) {
          const value = this.session.value(record.value);
          await this.session.root((ctx) => record.options.close!(value, ctx), true);
        }
      } finally {
        this.session.kernel.command(KernelOp.ResourceClosed, { resource: record.id });
        this.records.delete(record.id);
        this.closedRecords.set(record.id, record);
        this.session.flush();
      }
    })();
    return record.closed;
  }

  async reopen(record: ResourceRecord, id: number): Promise<void> {
    this.closedRecords.delete(record.id);
    record.reset(id);
    this.records.set(id, record);
    await record.start();
  }
}
