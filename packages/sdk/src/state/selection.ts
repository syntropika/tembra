import { protect } from "../values.ts";
import type { PreparedSelection, Readable } from "./types.ts";

export class Selection<T, R> implements Readable<R> {
  #value: Readonly<R>;
  #listeners = new Set<(value: Readonly<R>) => void>();
  #children = new Set<WeakRef<Selection<R, unknown>>>();
  #active = true;
  #cleanups: Set<() => void> | undefined;
  #cleanup: (() => void) | undefined;
  #remove: (() => void) | undefined;

  /** Compute and protect the initial selected value. */
  constructor(
    readonly compute: (value: Readonly<T>) => R,
    value: Readonly<T>,
    readonly report: (error: unknown) => void,
    readonly check: () => void,
  ) {
    this.#value = protect(compute(value));
  }

  /** Read the cached value while its owning reference remains valid. */
  get value(): Readonly<R> {
    this.check();
    return this.#value;
  }

  /** Attach weak cleanup so the owner can release this selection. */
  track(cleanups: Set<() => void> | undefined, remove: () => void): void {
    this.#cleanups = cleanups;
    this.#remove = remove;
    const weak = new WeakRef(this);
    this.#cleanup = () => weak.deref()?.detach();
    cleanups?.add(this.#cleanup);
  }

  /** Release listeners and child selections once. */
  detach(): void {
    if (!this.#active) return;
    this.#active = false;
    if (this.#cleanup) this.#cleanups?.delete(this.#cleanup);
    this.#remove?.();
    this.#listeners.clear();
    for (const weak of this.#children) weak.deref()?.detach();
    this.#children.clear();
  }

  /** Stage derived values before any state notification runs. */
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
      /** Adopt staged selections before returning their notification callbacks. */
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

  /** Derive a child selection with the same validity rule. */
  select<S>(compute: (value: Readonly<R>) => S): Readable<S> {
    this.check();
    const child = new Selection(compute, this.#value, this.report, this.check);
    this.check();
    const weak = new WeakRef(child) as WeakRef<Selection<R, unknown>>;
    this.#children.add(weak);
    child.track(this.#cleanups, () => this.#children.delete(weak));
    return child;
  }

  /** Deliver the current value and retain a change listener. */
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
