/**
 * One isolated component instance and the callback it currently serves.
 *
 * Every guest entry (a lifecycle callback, a service method, a contribution) is one admitted
 * callback: the revision's gate already serializes them, and the port additionally refuses
 * re-entry. Host imports resolve their authority from the callback active when they are called;
 * an asynchronous import keeps the callback it captured, never a later one. A callback ends only
 * after every host operation it started has physically settled. A timeout, an uncaught guest
 * exception or a trap poisons the instance, which is never entered again.
 *
 * Physical completion is the generated export task's real exit, observed through the trusted
 * compiler's exact instrumentation (`instrument.ts`): a callback keeps its admission and its
 * import attribution from entry until that exit, even after its result was returned at
 * `task.return`. A trap, or a task that exits rejected, quarantines the instance and rejects
 * every waiter racing the trap signal. No event-loop timing is used as a completion signal.
 */
import type { Handle } from "../host-protocol.ts";
import type { ExecutionScope } from "../types.ts";
import type { CoreLimits, TaskObserver } from "./compiler.ts";
import { abortable, type WasiHost } from "./wasi-host.ts";

/** The callback a host import acts for. */
export interface Callback {
  readonly scope: ExecutionScope;
  /** The host-issued environment of this callback (`host.environment`). */
  readonly environment: bigint;
  /** The one-turn dependency selection, consumed by the next matching import. */
  selection: { readonly kind: "reference" | "hook"; readonly handle: Handle } | undefined;
  /** Set when a selection was left unconsumed past its turn; the callback then fails. */
  stale: boolean;
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
    taskStarted: ({ id }) => {
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
      const callback = this.#tasks.get(id);
      if (!callback) return;
      this.#tasks.delete(id);
      // A rejected task left guest state the guest never observed: the instance cannot be reused.
      if (rejected || error !== undefined)
        this.poison(error ?? new Error("A component callback task ended rejected"));
      callback.exited();
    },
    trapped: (error) => this.poison(error),
  };
  #running = false;
  #closed = false;
  #poisoned = false;
  readonly #retired = new AbortController();
  /** Why the instance was quarantined, kept for the refusal of later entries. */
  #cause: unknown;
  /** The runtime's last stderr bytes (its panic text), attached only to quarantine causes. */
  #panic = "";

  /** Record runtime stderr for diagnostics; bounded to the last 4 KiB. */
  panic(bytes: Uint8Array): void {
    this.#panic = (this.#panic + new TextDecoder().decode(bytes)).slice(-4096);
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
      if (!callback) return abortable(work, this.#retired.signal);
      this.physical(callback, work);
      return abortable(abortable(work, callback.ended.signal), callback.scope.signal);
    },
    cleanup: (operation: () => unknown): void => {
      const callback = this.#attributed();
      const work = Promise.resolve().then(operation);
      this.#track(work);
      if (callback) this.physical(callback, work);
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

  /** Shared fuel and page budget of the instance, set at instantiation. */
  limits: CoreLimits | undefined;

  constructor(
    readonly timeout: number,
    readonly dispose: () => Promise<void>,
    /** Fuel granted to each admitted callback (never refilled for its continuations). */
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
      const owner = this.#tasks.get(id);
      if (!owner) throw new Error("Host import from a task outside any admitted callback");
      return owner;
    }
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

  /**
   * Record a one-turn selection; it must be consumed before the guest yields. `undefined` clears
   * a pending selection of the same callback (the guest's recovery after a recoverable argument
   * error thrown before its import was entered).
   */
  select(kind: "reference" | "hook", handle: Handle | undefined): void {
    const callback = this.current();
    if (handle === undefined) {
      if (callback.selection && callback.selection.kind !== kind) {
        callback.stale = true;
        throw new Error("The pending selection belongs to another import kind");
      }
      callback.selection = undefined;
      return;
    }
    if (callback.selection) {
      callback.stale = true;
      throw new Error("A previous selection was not consumed");
    }
    // A selection the callback could never use is refused now, recoverably.
    callback.scope.checkSelection?.(kind, handle);
    const selection = { kind, handle };
    callback.selection = selection;
    // The selecting turn ends when the guest returns to the host event loop.
    queueMicrotask(() => {
      if (callback.selection === selection) {
        callback.selection = undefined;
        callback.stale = true;
      }
    });
  }

  /** Take the selection for one import of `kind`. */
  take(callback: Callback, kind: "reference" | "hook"): Handle | undefined {
    const selection = callback.selection;
    callback.selection = undefined;
    if (selection && selection.kind !== kind) {
      callback.stale = true;
      throw new Error("The selection does not match this import");
    }
    return selection?.handle;
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
      if (this.#panic && cause instanceof Error)
        Object.defineProperty(cause, "runtimeStderr", {
          value: this.#panic.trim(),
        });
    }
    this.#poisoned = true;
    this.#retired.abort();
  }

  /**
   * Serve one admitted callback. `call` must turn recoverable WIT `result` errors into values;
   * anything it throws is a trap and poisons the instance.
   */
  /**
   * Serve one admitted callback and wait for its physical completion: the result, the task's real
   * exit and every host operation it started, all within one deadline.
   */
  async run<T>(environment: bigint, scope: ExecutionScope, call: () => T | Promise<T>): Promise<T> {
    const { result, finished } = this.start(environment, scope, call);
    const value = await result;
    await finished;
    return value;
  }

  /**
   * Enter one admitted callback. `result` settles with the export's value (an incoming HTTP
   * response head can be used while its body still streams); `finished` settles when the callback
   * physically ended: the task's real exit and the settlement of every host operation it started.
   * One deadline governs the whole lifetime: on expiry the instance is quarantined, the callback's
   * authority and waits are revoked, and provider work still running stays owned by the revision
   * until its disposal. `call` must turn recoverable WIT `result` errors into values; anything it
   * throws is a trap.
   */
  start<T>(
    environment: bigint,
    scope: ExecutionScope,
    call: () => T | Promise<T>,
  ): { result: Promise<T>; finished: Promise<void> } {
    if (this.#poisoned)
      throw new Error("The component revision is quarantined after a trap", {
        cause: this.#cause,
      });
    if (this.#closed || this.#running) throw new Error("Component is closed or busy");
    if (!this.exports) throw new Error("Component is not instantiated");
    this.#running = true;
    let exited!: () => void;
    const exit = new Promise<void>((resolve) => (exited = resolve));
    const callback: Callback = {
      scope,
      environment,
      selection: undefined,
      stale: false,
      pending: new Set(),
      ended: new AbortController(),
      task: undefined,
      exit,
      exited,
    };
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
    const end = () => {
      if (ended) return;
      ended = true;
      clearTimeout(timer);
      // Revoke the callback's authority: its waits and later host requests are refused.
      scope.cancel();
      callback.ended.abort();
      if (callback.task !== undefined) this.#tasks.delete(callback.task);
      this.#active = undefined;
      this.#running = false;
    };
    let pending: Promise<T>;
    try {
      scope.check();
      if (this.limits) this.limits.fuel.value = this.callFuel;
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
          end();
        }
        const error = new Error("No generated export task was observed for this callback");
        this.poison(error);
        throw error;
      })();
      return { result, finished: result.then(() => undefined) };
    }
    const result = bounded(pending).catch((error: unknown) => {
      this.poison(error);
      throw error;
    });
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
        end();
      }
      if (this.#poisoned)
        throw new Error("The component revision is quarantined after a trap", {
          cause: this.#cause,
        });
      // Refused like an unconsumed native arm; the instance itself is still consistent.
      if (callback.stale || callback.selection)
        throw new Error("A dependency selection was not consumed by its import in the same turn");
    })();
    result.catch(() => undefined);
    finished.catch(() => undefined);
    return { result, finished };
  }

  /** Dispose providers only after the last callback physically returned. */
  async close(): Promise<void> {
    if (this.#running) throw new Error("Guest still owns physical work");
    if (this.#closed) return;
    this.#closed = true;
    this.exports = undefined;
    this.#retired.abort();
    // Providers are disposed only after their last physical operation settled.
    while (this.#physical.size) await Promise.allSettled([...this.#physical]);
    await this.dispose();
  }
}
