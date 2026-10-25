import { TembraError, refuse } from "./error.ts";
import type { Invocation, Snapshot } from "./kernel.ts";
import { protect } from "./values.ts";

export interface Readable<T> {
  readonly value: Readonly<T>;
  select<R>(compute: (value: Readonly<T>) => R): Readable<R>;
  subscribe(listener: (value: Readonly<T>) => void): () => void;
}

export interface StateOptions<T> {
  version?: number;
  validate?: (candidate: Readonly<T>) => boolean | void;
  migrate?: (previous: unknown, fromVersion: number) => T;
}

export interface StateBackend {
  snapshot(object: number): Snapshot;
  value<T>(id: number): T;
  adopt(object: number, context: unknown, expected: string, candidate: unknown): Snapshot;
  check(context: unknown, object: number): Invocation;
  report(error: unknown): void;
  publish(operation: () => void): void;
}

export interface State<T> extends Readable<T> {
  update(context: unknown, change: (current: Readonly<T>) => T): Readonly<T>;
}

class Selection<T, R> implements Readable<R> {
  #value: Readonly<R>;
  #listeners = new Set<(value: Readonly<R>) => void>();
  #children = new Set<WeakRef<Selection<R, unknown>>>();
  #active = true;
  #cleanups: Set<() => void> | undefined;
  #cleanup: (() => void) | undefined;
  #remove: (() => void) | undefined;
  constructor(
    readonly compute: (value: Readonly<T>) => R,
    value: Readonly<T>,
    readonly report: (error: unknown) => void,
    readonly check: () => void,
  ) {
    this.#value = protect(compute(value));
  }
  get value(): Readonly<R> {
    this.check();
    return this.#value;
  }
  track(cleanups: Set<() => void> | undefined, remove: () => void): void {
    this.#cleanups = cleanups;
    this.#remove = remove;
    const weak = new WeakRef(this);
    this.#cleanup = () => weak.deref()?.detach();
    cleanups?.add(this.#cleanup);
  }
  detach(): void {
    if (!this.#active) return;
    this.#active = false;
    if (this.#cleanup) this.#cleanups?.delete(this.#cleanup);
    this.#remove?.();
    this.#listeners.clear();
    for (const weak of this.#children) weak.deref()?.detach();
    this.#children.clear();
  }
  prepare(value: Readonly<T>): PreparedSelection | undefined {
    if (!this.#active) return;
    try {
      this.check();
    } catch {
      this.detach();
      return;
    }
    let next: Readonly<R>;
    try {
      next = protect(this.compute(value));
    } catch (error) {
      this.detach();
      this.report(error);
      return;
    }
    if (!this.#active) return;
    const children: PreparedSelection[] = [];
    if (!Object.is(next, this.#value)) {
      for (const weak of this.#children) {
        const child = weak.deref();
        const prepared = child?.prepare(next);
        if (prepared) children.push(prepared);
        else this.#children.delete(weak);
      }
    }
    return {
      adopt: () => {
        if (!this.#active || Object.is(next, this.#value)) return () => {};
        this.#value = next;
        const notifyChildren = children.map((child) => child.adopt());
        const listeners = [...this.#listeners];
        return () => {
          if (!this.#active) return;
          for (const notify of notifyChildren) notify();
          for (const listener of listeners) {
            try {
              listener(next);
            } catch (error) {
              this.report(error);
            }
          }
        };
      },
    };
  }
  select<S>(compute: (value: Readonly<R>) => S): Readable<S> {
    this.check();
    const child = new Selection(compute, this.#value, this.report, this.check);
    this.check();
    const weak = new WeakRef(child) as WeakRef<Selection<R, unknown>>;
    this.#children.add(weak);
    child.track(this.#cleanups, () => this.#children.delete(weak));
    return child;
  }
  subscribe(listener: (value: Readonly<R>) => void): () => void {
    this.check();
    this.#listeners.add(listener);
    try {
      listener(this.#value);
    } catch (error) {
      this.report(error);
    }
    return () => {
      this.#listeners.delete(listener);
    };
  }
}

interface PreparedSelection {
  adopt(): () => void;
}

/** One object-owned state binding, shared by compatible implementation versions. */
export class StateBinding<T = unknown> {
  closed = false;
  #value: Readonly<T>;
  #listeners = new Set<(value: Readonly<T>) => void>();
  #selections = new Set<WeakRef<Selection<T, unknown>>>();
  constructor(
    readonly backend: StateBackend,
    readonly object: number,
    initial?: Readonly<T>,
  ) {
    this.#value =
      arguments.length >= 3 ? initial! : backend.value<T>(backend.snapshot(object).value);
  }
  check(): void {
    if (this.closed) throw new TembraError("gone", "state", "State owner is gone");
  }
  get value(): Readonly<T> {
    this.check();
    return this.#value;
  }

  view(options: StateOptions<T>, check: () => void = () => this.check()): State<T> {
    const binding = this;
    return {
      get value() {
        check();
        return binding.value;
      },
      update(context, change) {
        check();
        binding.backend.check(context, binding.object);
        const previous = binding.backend.snapshot(binding.object);
        const current = binding.backend.value<T>(previous.value);
        const candidate = change(current);
        validate(candidate, options);
        binding.backend.check(context, binding.object);
        if (Object.is(candidate, current)) return current;
        const selected = binding.prepare(candidate);
        binding.backend.adopt(binding.object, context, previous.revision, candidate);
        const publish = binding.adopt(selected, candidate);
        binding.backend.publish(publish);
        return candidate;
      },
      select: (compute) => {
        check();
        return binding.select(compute, check);
      },
      subscribe: (listener) => {
        check();
        return binding.subscribe(listener);
      },
    };
  }
  select<R>(
    compute: (value: Readonly<T>) => R,
    check = () => this.check(),
    cleanups?: Set<() => void>,
  ): Readable<R> {
    check();
    const selection = new Selection(
      compute,
      this.value,
      (error) => this.backend.report(error),
      check,
    );
    check();
    const weak = new WeakRef(selection) as WeakRef<Selection<T, unknown>>;
    this.#selections.add(weak);
    selection.track(cleanups, () => this.#selections.delete(weak));
    return selection;
  }
  subscribe(listener: (value: Readonly<T>) => void): () => void {
    this.check();
    this.#listeners.add(listener);
    try {
      listener(this.value);
    } catch (error) {
      this.backend.report(error);
    }
    return () => {
      this.#listeners.delete(listener);
    };
  }
  prepare(candidate: Readonly<T>): PreparedSelection[] {
    const prepared: PreparedSelection[] = [];
    for (const weak of this.#selections) {
      const selection = weak.deref();
      const next = selection?.prepare(candidate);
      if (next) prepared.push(next);
      else this.#selections.delete(weak);
    }
    return prepared;
  }
  adopt(prepared: PreparedSelection[], candidate: Readonly<T>): () => void {
    this.#value = candidate;
    const selections = prepared.map((p) => p.adopt());
    const listeners = [...this.#listeners];
    return () => {
      for (const notify of selections) notify();
      for (const listener of listeners) {
        try {
          listener(candidate);
        } catch (error) {
          this.backend.report(error);
        }
      }
    };
  }
  close(): void {
    this.closed = true;
    this.#value = undefined as T;
    this.#listeners.clear();
    for (const weak of this.#selections) weak.deref()?.detach();
    this.#selections.clear();
  }
}

export function validate<T>(candidate: T, options: StateOptions<T>): Readonly<T> {
  if (candidate && typeof (candidate as { then?: unknown }).then === "function")
    refuse("async_state", "State updates and migrations must be synchronous");
  const protectedValue = protect(candidate);
  const result = options.validate?.(protectedValue);
  if (result !== undefined && result !== true)
    refuse("state_validation", "Facet rejected the candidate state");
  return protectedValue;
}
