import { Errors } from "../errors.ts";

import { TembraError, refuse } from "../error.ts";
import { protect } from "../values.ts";

import { Selection } from "./selection.ts";
import type { PreparedSelection, Readable, State, StateBackend, StateOptions } from "./types.ts";

/** One object-owned state binding, shared by compatible implementation versions. */
export class StateBinding<T = unknown> {
  closed = false;
  #value: Readonly<T>;
  #listeners = new Set<(value: Readonly<T>) => void>();
  #selections = new Set<WeakRef<Selection<T, unknown>>>();

  /** Bind an object's current state, optionally using a staged initial value. */
  constructor(
    readonly backend: StateBackend,
    readonly object: number,
    initial?: Readonly<T>,
  ) {
    this.#value =
      arguments.length >= 3 ? initial! : backend.value<T>(backend.snapshot(object).value);
  }

  /** Reject state access after object cleanup. */
  check(): void {
    if (this.closed) throw new TembraError(Errors.Gone.State, "State owner is gone");
  }

  /** Read current state while its owner remains live. */
  get value(): Readonly<T> {
    this.check();
    return this.#value;
  }

  /** Expose validated updates under the declaring facet's state contract. */
  view(options: StateOptions<T>, check: () => void = () => this.check()): State<T> {
    const binding = this;
    return {
      /** Read adopted state after checking the view's lifetime. */
      get value() {
        check();
        return binding.value;
      },
      /** Validate a synchronous candidate before revision-checked adoption. */
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
      /** Derive a selection after checking the view's lifetime. */
      select: (compute) => {
        check();
        return binding.select(compute, check);
      },
      /** Attach an observer only while this view remains valid. */
      subscribe: (listener) => {
        check();
        return binding.subscribe(listener);
      },
    };
  }

  /** Track a derived value without retaining unused selections strongly. */
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

  /** Deliver current state immediately and report listener failures. */
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

  /** Compute live selections before changing the adopted value. */
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

  /** Replace state and return its deferred notification pass. */
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

  /** Release state and detach all tracked observers. */
  close(): void {
    this.closed = true;
    this.#value = undefined as T;
    this.#listeners.clear();
    for (const weak of this.#selections) weak.deref()?.detach();
    this.#selections.clear();
  }
}

/** Protect plain data and apply the facet's synchronous guard. */
export function validate<T>(candidate: T, options: StateOptions<T>): Readonly<T> {
  if (candidate && typeof (candidate as { then?: unknown }).then === "function")
    refuse(Errors.Invalid.AsyncState, "State updates and migrations must be synchronous");
  const protectedValue = protect(candidate);
  const result = options.validate?.(protectedValue);
  if (result !== undefined && result !== true)
    refuse(Errors.Invalid.StateValidation, "Facet rejected the candidate state");
  return protectedValue;
}
