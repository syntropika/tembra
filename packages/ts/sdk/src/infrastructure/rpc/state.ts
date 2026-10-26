import type { Readable } from "../../state.ts";
import { protect } from "../../values.ts";

export class RemoteState implements Readable<any> {
  #value: any;
  #listeners = new Set<(value: any) => void>();

  /** Protect the initial remote snapshot and retain its validity check. */
  constructor(
    value: unknown,
    readonly check: () => void,
    readonly report: (error: unknown) => void,
  ) {
    this.#value = protect(value);
  }

  /** Read the snapshot while the remote reference remains valid. */
  get value(): any {
    this.check();
    return this.#value;
  }

  /** Publish a protected remote snapshot to local observers. */
  adopt(value: unknown): void {
    this.#value = protect(value);
    for (const listener of this.#listeners) {
      try {
        listener(this.#value);
      } catch (error) {
        this.report(error);
      }
    }
  }

  /** Deliver the current snapshot, then subsequent updates. */
  subscribe(listener: (value: any) => void): () => void {
    this.check();
    this.#listeners.add(listener);
    listener(this.#value);
    return () => {
      this.#listeners.delete(listener);
    };
  }

  /** Derive a readable value from the remote state. */
  select<R>(compute: (value: any) => R): Readable<R> {
    const source = this;
    return {
      /** Compute the selected value from the current remote snapshot. */
      get value() {
        return compute(source.value);
      },
      /** Compose another projection over the same remote source. */
      select: (next) => source.select((value) => next(compute(value))),
      /** Notify only when the selected value changes by identity. */
      subscribe(listener) {
        let initial = true;
        let previous: R;
        return source.subscribe((value) => {
          const next = compute(value);
          if (initial || !Object.is(previous, next)) {
            initial = false;
            previous = next;
            listener(next);
          }
        });
      },
    };
  }
}
