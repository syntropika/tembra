import { EnvironmentPhase, KernelOp } from "../bridge/protocol.ts";
import { Errors } from "../errors.ts";

import { refuse, TembraError } from "../error.ts";
import type { Environment, ListenerRecord, TaskDefinition, TaskRecord } from "./records.ts";
import type { Session } from "./session.ts";

export class Activities {
  readonly tasks = new Map<number, TaskRecord>();
  readonly listeners = new Map<number, ListenerRecord>();
  #identities = new WeakMap<object, number>();

  /** Keep task and listener callbacks for this session. */
  constructor(readonly session: Session) {}

  /** Attach declared sources using subscription identities issued by Rust. */
  prepare(environment: Environment, previous?: Environment): void {
    for (const definition of environment.listeners) {
      const source =
        typeof definition.source === "function" ? definition.source() : definition.source;
      if (!source || typeof source.subscribe !== "function") refuse(Errors.Invalid.EventSource);
      const id = this.session.kernel.command<number>(KernelOp.Listen, {
        scope: environment.id,
        previous: previous?.id,
        name: definition.name,
        source: this.identity(source),
        sizing: definition.options.sizeOf ? this.identity(definition.options.sizeOf) : 0,
        operation: this.session.kernel.command<number>(KernelOp.Allocate),
        maxPending: definition.options.maxPending ?? 64,
        ...(definition.options.maxBytes ? { maxBytes: definition.options.maxBytes } : {}),
        coalesce: "select" in source,
      });
      const existing = this.listeners.get(id);
      if (existing) {
        definition.running = existing;
        continue;
      }
      const record: ListenerRecord = {
        id,
        definition,
        environment,
        stopped: false,
        /** Allow safe stop before the source returns its detach callback. */
        detach() {},
      };
      definition.running = record;
      this.listeners.set(id, record);
      try {
        record.detach = source.subscribe((value) => {
          if (record.stopped) return;
          const payload = this.session.values.put(value);
          try {
            const bytes = definition.options.sizeOf?.(value) ?? 0;
            if (!Number.isSafeInteger(bytes) || bytes < 0 || bytes > 0xffff_ffff)
              throw new TembraError(Errors.Invalid.SubscriptionSize);
            this.session.kernel.command(KernelOp.Enqueue, { subscription: id, payload, bytes });
          } catch (error) {
            this.session.values.drop(payload);
            this.session.report(error);
            this.failed(id);
          }
          this.session.lifecycle.flush();
        });
        if (record.stopped) record.detach();
      } catch (error) {
        this.stopListener(record);
        throw error;
      }
    }
  }

  /** Start tasks and retarget listeners after scope adoption. */
  start(environment: Environment): void {
    environment.phase = EnvironmentPhase.Live;
    for (const definition of environment.listeners) {
      const record = definition.running!;
      record.environment = environment;
      record.definition = definition;
    }
    for (const task of environment.tasks) this.startTask(environment, task);
  }

  /** Run one task until its physical callback settles. */
  private startTask(environment: Environment, definition: TaskDefinition): void {
    if (definition.running || environment.phase !== EnvironmentPhase.Live || this.session.closing)
      return;
    const id = this.session.kernel.command<number>(KernelOp.Task, { owner: environment.owner });
    const controller = new AbortController();
    const operation = this.session.kernel.command<number>(KernelOp.Allocate);
    const record: TaskRecord = { id, controller, done: Promise.resolve() };
    definition.running = record;
    this.tasks.set(id, record);
    record.done = this.session.track(
      Promise.resolve().then(async () => {
        try {
          await definition.start({
            signal: controller.signal,
            /** Issue operation authority under the task's retained version. */
            root: (callback) => {
              const root = this.session.kernel.command<number>(KernelOp.TaskRoot, {
                task: id,
                principal: this.session.principalFor(environment.object?.id ?? 0),
              });
              return this.session.calls.root(
                () => this.session.calls.owned(environment, operation, callback, root),
                false,
                root,
              );
            },
          });
        } catch (error) {
          if (!controller.signal.aborted) this.session.report(error);
        } finally {
          this.session.kernel.command(KernelOp.Tick, { now: this.session.now() });
          this.session.kernel.command(KernelOp.FinishTask, { task: id });
          this.tasks.delete(id);
          delete definition.running;
          this.session.lifecycle.flush();
        }
      }),
    );
  }

  /** Reuse one kernel identity for the same source or sizing callback. */
  private identity(value: object): number {
    let id = this.#identities.get(value);
    if (id === undefined) {
      id = this.session.kernel.command<number>(KernelOp.Allocate);
      this.#identities.set(value, id);
    }
    return id;
  }

  /** Execute an admitted delivery and acknowledge physical settlement. */
  deliver(subscription: number, payload: number, operation: number): void {
    const record = this.listeners.get(subscription);
    const job = Promise.resolve().then(async () => {
      try {
        if (!record) throw new TembraError(Errors.Gone.ListenerCallback);
        await this.session.calls.owned(record.environment, operation, (ctx) =>
          record.definition.handle(ctx, this.session.values.get(payload)),
        );
      } catch (error) {
        this.session.report(error);
      } finally {
        this.session.kernel.command(KernelOp.FinishDelivery, { subscription });
        this.session.lifecycle.flush();
      }
    });
    if (record) record.done = job;
    this.session.track(job);
  }

  /** Stop a listener selected by the kernel. */
  detach(id: number): void {
    const record = this.listeners.get(id);
    if (record) this.stopListener(record);
  }

  /** Signal task cancellation without releasing its ownership. */
  cancel(id: number): void {
    this.tasks.get(id)?.controller.abort(new TembraError(Errors.Cancelled.Task));
  }

  /** Report queue exhaustion and stop its source listener. */
  failed(id: number): void {
    const record = this.listeners.get(id);
    if (!record) return;
    this.session.report(new TembraError(Errors.Exhausted.SubscriptionQueue));
    this.stopListener(record);
  }

  /** Detach the source and release its kernel subscription once. */
  private stopListener(record: ListenerRecord): void {
    if (record.stopped) return;
    record.stopped = true;
    try {
      record.detach();
    } catch (error) {
      this.session.report(error);
    }
    this.session.kernel.command(KernelOp.Unsubscribe, { subscription: record.id });
    this.listeners.delete(record.id);
    this.session.lifecycle.flush();
  }

  /** Wait for owned tasks and admitted deliveries to settle. */
  async stop(environment: Environment): Promise<void> {
    for (const definition of environment.tasks) {
      if (definition.running)
        this.session.kernel.command(KernelOp.StopTask, { task: definition.running.id });
    }
    this.session.lifecycle.flush();
    await Promise.all(environment.tasks.map((task) => task.running?.done));
    await Promise.all(environment.listeners.map((listener) => listener.running?.done));
  }
}
