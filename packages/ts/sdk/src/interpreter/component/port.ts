/**
 * One isolated component instance and the callback it currently serves.
 *
 * Every guest entry (a service method, a contribution, an incoming HTTP request) is one admitted
 * callback: the revision's gate serializes entry. A published byte-stream head can park its
 * callback so another entry can proceed while its producer remains physically owned. Host
 * imports resolve their authority from the callback active when they are called;
 * an asynchronous import keeps the callback it captured, never a later one. A callback ends only
 * after every host operation it started has physically settled. A timeout, an uncaught guest
 * exception or a trap poisons the instance, which is never entered again.
 *
 * Physical completion is the generated export task's real exit, observed through the trusted
 * glue's exact build-time instrumentation (`tooling/component-glue.ts`): a callback keeps its admission and its
 * import attribution from entry until that exit, even after its result was returned at
 * `task.return`. A trap, or a task that exits rejected, quarantines the instance and rejects
 * every waiter racing the trap signal. No event-loop timing is used as a completion signal.
 */
import type { CoreLimits, TaskObserver } from "./instance.ts";
import type { CallbackScope } from "./scope.ts";
import { abortable, type WasiHost } from "./wasi-host.ts";

/** The callback a host import acts for. */
export interface Callback {
  readonly scope: CallbackScope;
  /** The host-issued environment of this callback. */
  readonly environment: bigint;
  /** Physical host operations started by this callback. */
  readonly pending: Set<Promise<unknown>>;
  /** Aborts when the callback ends; its waiters stop while physical work stays tracked. */
  readonly ended: AbortController;
  /** The generated export task this callback entered, bound synchronously at entry. */
  task: number | undefined;
  /** Resolves at that task's real exit. */
  readonly exit: Promise<void>;
  readonly exited: () => void;
}

export class ComponentPort {
  exports: Record<string, Record<string, (...args: unknown[]) => unknown>> | undefined;
  #active: Callback | undefined;
  /** All physically live callbacks, including parked stream producers. */
  readonly #callbacks = new Set<Callback>();
  readonly #completions = new Set<Promise<void>>();
  #destroying = false;
  readonly #destructorTasks = new Set<number>();
  readonly #destructorPending = new Set<Promise<unknown>>();
  #destructorExited: (() => void) | undefined;
  /** The callback being entered right now; its export task starts synchronously. */
  #admitting: Callback | undefined;
  /** Whether any callback was admitted; earlier tasks are the instance's own initialization. */
  #admitted = false;
  /** Export tasks of admitted callbacks until their exit. */
  readonly #tasks = new Map<number, Callback>();
  #access: { currentRootTask(): number | null } | undefined;
  /** Rejects once the instance is quarantined; waiters race it. */
  readonly #trap: Promise<never>;
  #rejectTrap!: (error: unknown) => void;

  /** Lifecycle notifications from the instrumented glue of this instance. */
  readonly observer: TaskObserver = {
    attach: (access) => {
      this.#access = access;
    },
    taskStarted: ({ id, entry }) => {
      if (this.#destroying) {
        if (entry !== "<resource-drop>") {
          this.poison(new Error("A destructor entered a non-destructor export task"));
          return;
        }
        this.#destructorTasks.add(id);
        return;
      }
      const callback = this.#admitting;
      if (!callback) {
        // Instantiation runs the component's own initialization before any admission.
        if (this.#admitted)
          this.poison(new Error("An export task started outside an admitted callback"));
        return;
      }
      if (callback.task !== undefined) {
        this.poison(new Error("One callback entered more than one export task"));
        return;
      }
      callback.task = id;
      this.#tasks.set(id, callback);
    },
    taskExited: ({ id, rejected, error }) => {
      if (this.#destructorTasks.delete(id)) {
        if (rejected || error !== undefined)
          this.poison(error ?? new Error("A component resource destructor ended rejected"));
        if (!this.#destructorTasks.size) this.#destructorExited?.();
        return;
      }
      const callback = this.#tasks.get(id);
      if (!callback) return;
      // A rejected task left guest state the guest never observed: the instance cannot be reused.
      if (rejected || error !== undefined)
        this.poison(error ?? new Error("A component callback task ended rejected"));
      callback.exited();
    },
    trapped: (error) => this.poison(error),
  };
  #closed = false;
  #poisoned = false;
  readonly #retired = new AbortController();
  /** Why the instance was quarantined, kept for the refusal of later entries. */
  #cause: unknown;
  /** Whether the instrumented glue attached its task accessor during instantiation. */
  get attached(): boolean {
    return this.#access !== undefined;
  }
  /** Every provider operation of this revision, settled before providers are disposed. */
  readonly #physical = new Set<Promise<unknown>>();

  /** Services of the WASI adapters bound to this instance. */
  readonly wasi: WasiHost = {
    physical: <T>(operation: () => Promise<T>): Promise<T> => {
      if (this.#retired.signal.aborted)
        return Promise.reject(new Error("The revision has retired"));
      const work = Promise.resolve().then(operation);
      this.#track(work);
      const callback = this.#attributed();
      if (!callback) {
        this.#destructorPhysical(work);
        return abortable(work, this.#retired.signal);
      }
      this.physical(callback, work);
      return abortable(abortable(work, callback.ended.signal), callback.scope.signal);
    },
    cleanup: (operation: () => unknown): void => {
      const callback = this.#attributed();
      const work = Promise.resolve().then(operation);
      this.#track(work);
      if (callback) this.physical(callback, work);
      else this.#destructorPhysical(work);
      // A destructor has no result lane. Its failure must not disappear after a successful
      // export result, or let the next callback reuse an incompletely closed resource.
      work.catch((error: unknown) => this.poison(error));
    },
    retired: this.#retired.signal,
    signal: (): AbortSignal => {
      const callback = this.#attributed();
      const signals = [this.#retired.signal];
      if (callback) {
        signals.push(callback.ended.signal);
        if (callback.scope.signal) signals.push(callback.scope.signal);
      }
      return AbortSignal.any(signals);
    },
  };

  #track(work: Promise<unknown>): void {
    this.#physical.add(work);
    const forget = () => this.#physical.delete(work);
    work.then(forget, forget);
  }

  #destructorPhysical(work: Promise<unknown>): void {
    const id = this.#access?.currentRootTask();
    if (id === null || id === undefined || !this.#destructorTasks.has(id)) return;
    this.#destructorPending.add(work);
    const forget = () => this.#destructorPending.delete(work);
    work.then(forget, forget);
  }

  /** Shared fuel and page budget of the instance, set at instantiation. */
  limits: CoreLimits | undefined;

  constructor(
    readonly timeout: number,
    readonly dispose: () => Promise<void>,
    /** Fuel shared by a physically overlapping callback group; continuations never refill it. */
    readonly callFuel: bigint = 100_000_000n,
  ) {
    this.#trap = new Promise<never>((_, reject) => (this.#rejectTrap = reject));
    this.#trap.catch(() => {});
  }

  /**
   * The callback a host import serves: the admitted callback owning the guest's current root
   * export task (read synchronously at import entry), or the active callback for host-side work
   * outside guest code. There is none between callbacks.
   */
  current(): Callback {
    const id = this.#access?.currentRootTask();
    if (id !== null && id !== undefined) {
      if (this.#destructorTasks.has(id))
        throw new Error("A resource destructor holds no call authority");
      const owner = this.#tasks.get(id);
      if (!owner) throw new Error("Host import from a task outside any admitted callback");
      return owner;
    }
    if (this.#destroying) throw new Error("A resource destructor holds no call authority");
    const active = this.#active;
    if (!active) throw new Error("No active host scope");
    return active;
  }

  #attributed(): Callback | undefined {
    try {
      return this.current();
    } catch {
      return undefined;
    }
  }

  /** Queue revision-owned destruction; it must never wait on the callback holding its gate. */
  cleanup(operation: () => Promise<void>): Promise<void> {
    const work = Promise.resolve().then(operation);
    this.#track(work);
    work.catch((error: unknown) => this.poison(error));
    return work;
  }

  /** Run a guest destructor without borrowing the active callback's authority. */
  async destroy(call: () => void): Promise<void> {
    if (!this.usable) return;
    if (this.#destroying) throw new Error("A resource destructor is already active");
    this.#destroying = true;
    const exited = Promise.withResolvers<void>();
    this.#destructorExited = exited.resolve;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<never>((_, reject) => {
      timer = setTimeout(
        () => reject(new Error("Resource destructor exceeded its wall time limit")),
        this.timeout,
      );
    });
    const physical = (async () => {
      await exited.promise;
      while (this.#destructorPending.size) await Promise.allSettled([...this.#destructorPending]);
    })();
    let failure: { error: unknown } | undefined;
    try {
      if (this.limits && !this.#callbacks.size) this.limits.fuel.value = this.callFuel;
      call();
      if (!this.#destructorTasks.size) exited.resolve();
      // Instrumentation reports the exact task exit through its exit promise. Even a
      // synchronous canonical destructor delivers that notification asynchronously.
      await Promise.race([physical, deadline, this.#trap]);
      if (this.#poisoned) throw this.#cause ?? new Error("A resource destructor trapped");
    } catch (error) {
      this.poison(error);
      failure = { error };
    } finally {
      clearTimeout(timer);
      if (!this.#destructorTasks.size) exited.resolve();
      // A timeout poisons immediately, but a resource's cleanup and anchor remain retained
      // until its actual destructor task and the provider work it started have ended.
      await physical;
      this.#destructorTasks.clear();
      this.#destructorExited = undefined;
      this.#destroying = false;
    }
    if (failure) throw failure.error;
  }

  /** Race a waiter against the quarantine signal. */
  guard<T>(operation: Promise<T>): Promise<T> {
    return Promise.race([operation, this.#trap]);
  }

  /** Account one physical host operation to its callback until it settles. */
  physical<T>(callback: Callback, operation: Promise<T>): Promise<T> {
    callback.pending.add(operation);
    const forget = () => callback.pending.delete(operation);
    operation.then(forget, forget);
    return operation;
  }

  /** Whether the instance can still be entered. */
  get usable(): boolean {
    return !this.#closed && !this.#poisoned;
  }

  /** Mark the instance unusable after a trap observed outside `run`. */
  poison(cause?: unknown): void {
    if (!this.#poisoned) {
      // Waiters see the actual trap; later entries are refused as quarantined.
      this.#rejectTrap(cause ?? new Error("The component revision is quarantined after a trap"));
      this.#cause = cause;
    }
    this.#poisoned = true;
    this.#retired.abort();
  }

  /**
   * Serve one callback through its bounded outcome, joining actual completion on success.
   * Register physical retention before waiting: a failure can answer promptly while its task
   * and providers are still physically owned by the invocation.
   */
  async run<T>(
    environment: bigint,
    scope: CallbackScope,
    call: () => T | Promise<T>,
    retain?: (physical: Promise<void>) => void,
  ): Promise<T> {
    const { result, finished, physical } = this.start(environment, scope, call);
    retain?.(physical);
    const value = await result;
    await finished;
    await physical;
    return value;
  }

  /**
   * Enter one admitted callback. `result` settles with the export's value (an incoming HTTP
   * response head can be used while its body still streams). `finished` is the bounded callback
   * outcome; `physical` proves actual task exit and settlement of its host work, even if a
   * timeout or trap already rejected `finished`. Retention uses `physical`, never the deadline.
   * One deadline governs the whole lifetime: on expiry the instance is quarantined, the callback's
   * authority and waits are revoked, and provider work still running stays owned by the revision
   * until its disposal. `call` must turn recoverable WIT `result` errors into values; anything it
   * throws is a trap. After a successful checked stream head, `park()` releases the exclusive
   * entry slot while preserving this callback's physical lifetime and deadline. Overlapping
   * callbacks and destructors share the group's original fuel until every callback has ended.
   */
  start<T>(
    environment: bigint,
    scope: CallbackScope,
    call: () => T | Promise<T>,
  ): { result: Promise<T>; finished: Promise<void>; physical: Promise<void>; park(): void } {
    if (this.#poisoned)
      throw new Error("The component revision is quarantined after a trap", {
        cause: this.#cause,
      });
    if (this.#closed || this.#active || this.#destroying)
      throw new Error("Component is closed or busy");
    if (!this.exports) throw new Error("Component is not instantiated");
    const newGroup = this.#callbacks.size === 0;
    let exited!: () => void;
    const exit = new Promise<void>((resolve) => (exited = resolve));
    const callback: Callback = {
      scope,
      environment,
      pending: new Set(),
      ended: new AbortController(),
      task: undefined,
      exit,
      exited,
    };
    this.#callbacks.add(callback);
    this.#active = callback;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<never>((_, reject) => {
      timer = setTimeout(() => {
        const error = new Error("Component callback exceeded its wall time limit");
        this.poison(error);
        reject(error);
      }, this.timeout);
    });
    deadline.catch(() => {});
    const bounded = <V>(operation: Promise<V>) => Promise.race([operation, deadline, this.#trap]);
    let ended = false;
    const revoke = () => {
      if (ended) return;
      ended = true;
      clearTimeout(timer);
      // Revoke the callback's authority: its waits and later host requests are refused.
      scope.cancel();
      callback.ended.abort();
      if (this.#active === callback) this.#active = undefined;
    };
    const release = () => {
      if (callback.task !== undefined) this.#tasks.delete(callback.task);
      this.#callbacks.delete(callback);
    };
    let pending: Promise<T>;
    try {
      scope.check();
      if (this.limits && newGroup) this.limits.fuel.value = this.callFuel;
      // The export task starts synchronously inside the generated wrapper and binds here.
      this.#admitting = callback;
      this.#admitted = true;
      try {
        pending = Promise.resolve(call());
      } finally {
        this.#admitting = undefined;
      }
    } catch (error) {
      pending = Promise.reject(error);
    }
    if (callback.task === undefined) {
      // The guest was never entered (a host-side refusal): nothing to quarantine.
      const result = (async () => {
        try {
          await pending;
        } finally {
          revoke();
          release();
        }
        const error = new Error("No generated export task was observed for this callback");
        this.poison(error);
        throw error;
      })();
      const finished = result.then(() => undefined);
      this.#completion(finished);
      return {
        result,
        finished,
        physical: finished,
        park() {
          throw new Error("No successful component result to park");
        },
      };
    }
    let published = false;
    const result = bounded(pending).then(
      (value) => {
        published = true;
        return value;
      },
      (error: unknown) => {
        this.poison(error);
        throw error;
      },
    );
    const finished = (async () => {
      try {
        await result.catch(() => undefined);
        // A result is not physical completion: admission and attribution stay with this
        // callback until its task really exits and its host operations settled.
        await bounded(callback.exit);
        while (callback.pending.size) await bounded(Promise.allSettled([...callback.pending]));
      } catch (error) {
        this.poison(error);
        throw error;
      } finally {
        revoke();
      }
      if (this.#poisoned)
        throw new Error("The component revision is quarantined after a trap", {
          cause: this.#cause,
        });
    })();
    result.catch(() => undefined);
    finished.catch(() => undefined);
    const physical = (async () => {
      try {
        // Quarantine revokes waiters promptly; it is not evidence that a suspended stack or
        // its providers exited. Retain task identity and its kernel owner until both did.
        await pending.catch(() => undefined);
        await callback.exit;
        while (callback.pending.size) await Promise.allSettled([...callback.pending]);
        await finished;
      } finally {
        release();
      }
    })();
    physical.catch(() => undefined);
    this.#completion(physical);
    return {
      result,
      finished,
      physical,
      park: () => {
        if (!published) throw new Error("No successful component result to park");
        if (this.#active === callback) this.#active = undefined;
      },
    };
  }

  #completion(work: Promise<void>): void {
    this.#completions.add(work);
    const forget = () => this.#completions.delete(work);
    work.then(forget, forget);
  }

  /** Join physical provider work and resource destruction without retiring the instance. */
  async settled(): Promise<void> {
    while (this.#completions.size || this.#physical.size)
      await Promise.allSettled([...this.#completions, ...this.#physical]);
  }

  /** Dispose providers only after the last callback physically returned. */
  async close(): Promise<void> {
    if (this.#callbacks.size || this.#destroying) throw new Error("Guest still owns physical work");
    if (this.#closed) return;
    this.#closed = true;
    this.exports = undefined;
    this.#retired.abort();
    // Providers are disposed only after their last physical operation settled.
    while (this.#physical.size) await Promise.allSettled([...this.#physical]);
    await this.dispose();
  }
}
