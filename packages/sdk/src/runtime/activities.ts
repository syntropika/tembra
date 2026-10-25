import { refuse, TembraError } from "../error.ts";
import type { Environment, ListenerRecord, TaskDefinition, TaskRecord } from "./records.ts";
import type { Session } from "./session.ts";

export class Activities {
  readonly tasks = new Map<number, TaskRecord>();
  readonly listeners = new Map<number, ListenerRecord>();
  #identities = new WeakMap<object, number>();
  constructor(readonly session: Session) {}

  prepare(environment: Environment, previous?: Environment): void {
    for (const definition of environment.listeners) {
      const source =
        typeof definition.source === "function" ? definition.source() : definition.source;
      if (!source || typeof source.subscribe !== "function") refuse("event_source");
      const id = this.session.kernel.command<number>("listen", {
        scope: environment.id,
        previous: previous?.id,
        name: definition.name,
        source: this.identity(source),
        sizing: definition.options.sizeOf ? this.identity(definition.options.sizeOf) : 0,
        operation: this.session.kernel.command<number>("allocate"),
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
        source,
        definition,
        environment,
        stopped: false,
        active: false,
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
              throw new TembraError("invalid", "subscription_size");
            this.session.kernel.command("enqueue", { subscription: id, payload, bytes });
          } catch (error) {
            this.session.values.drop(payload);
            this.session.report(error);
            this.failed(id);
          }
          this.session.flush();
        });
        if (record.stopped) record.detach();
      } catch (error) {
        this.stopListener(record);
        throw error;
      }
    }
  }

  start(environment: Environment): void {
    environment.phase = "live";
    for (const definition of environment.listeners) {
      const record = definition.running!;
      record.environment = environment;
      record.definition = definition;
      record.active = true;
    }
    for (const task of environment.tasks) this.startTask(environment, task);
  }

  private startTask(environment: Environment, definition: TaskDefinition): void {
    if (definition.running || environment.phase !== "live" || this.session.closing) return;
    const id = this.session.kernel.command<number>("task", { owner: environment.owner });
    const controller = new AbortController();
    const operation = this.session.kernel.command<number>("allocate");
    const record: TaskRecord = { id, controller, done: Promise.resolve() };
    definition.running = record;
    this.tasks.set(id, record);
    record.done = this.session.track(
      Promise.resolve().then(async () => {
        try {
          await definition.start({
            signal: controller.signal,
            root: (callback) => {
              const root = this.session.kernel.command<number>("task-root", {
                task: id,
                principal: this.session.principalFor(environment.object?.id ?? 0),
              });
              return this.session.root(
                () => this.session.owned(environment, operation, callback, root),
                false,
                root,
              );
            },
          });
        } catch (error) {
          if (!controller.signal.aborted) this.session.report(error);
        } finally {
          this.session.kernel.command("tick", { now: this.session.now() });
          this.session.kernel.command("finish-task", { task: id });
          this.tasks.delete(id);
          delete definition.running;
          this.session.flush();
          this.session.scheduleLinger();
        }
      }),
    );
  }

  private identity(value: object): number {
    let id = this.#identities.get(value);
    if (id === undefined) {
      id = this.session.kernel.command<number>("allocate");
      this.#identities.set(value, id);
    }
    return id;
  }

  deliver(subscription: number, payload: number, operation: number): void {
    const record = this.listeners.get(subscription);
    const job = Promise.resolve().then(async () => {
      try {
        if (!record) throw new TembraError("gone", "listener_callback");
        await this.session.owned(record.environment, operation, (ctx) =>
          record.definition.handle(ctx, this.session.values.get(payload)),
        );
      } catch (error) {
        this.session.report(error);
      } finally {
        this.session.kernel.command("finish-delivery", { subscription });
        this.session.flush();
      }
    });
    if (record) record.done = job;
    this.session.track(job);
  }

  detach(id: number): void {
    const record = this.listeners.get(id);
    if (record) this.stopListener(record);
  }

  cancel(id: number): void {
    this.tasks.get(id)?.controller.abort(new TembraError("cancelled", "task"));
  }

  failed(id: number): void {
    const record = this.listeners.get(id);
    if (!record) return;
    this.session.report(new TembraError("exhausted", "subscription_queue"));
    this.stopListener(record);
  }

  private stopListener(record: ListenerRecord): void {
    if (record.stopped) return;
    record.stopped = true;
    record.active = false;
    try {
      record.detach();
    } catch (error) {
      this.session.report(error);
    }
    this.session.kernel.command("unsubscribe", { subscription: record.id });
    this.listeners.delete(record.id);
    this.session.flush();
  }

  async stop(environment: Environment, _keepListeners = false): Promise<void> {
    for (const definition of environment.tasks) {
      if (definition.running)
        this.session.kernel.command("stop-task", { task: definition.running.id });
    }
    this.session.flush();
    await Promise.all(environment.tasks.map((task) => task.running?.done));
    await Promise.all(environment.listeners.map((listener) => listener.running?.done));
  }
}
