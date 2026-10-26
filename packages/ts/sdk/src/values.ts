import { Errors } from "./errors.ts";

import { refuse } from "./error.ts";

/** Owned bridge slots. Local values never pass through the metadata kernel or a codec. */
export class Values {
  #next = 1;
  #values = new Map<number, unknown>();

  /** Use the shared kernel allocator when values cross language adapters. */
  constructor(readonly allocate?: () => number) {}

  /** Retain a value under a fresh bridge identity. */
  put(value: unknown): number {
    if (this.#next > 0xffff_ffff) refuse(Errors.Invalid.ValuesExhausted);
    const id = this.allocate ? this.allocate() : this.#next++;
    this.#values.set(id, value);
    return id;
  }

  /** Resolve a retained value or reject an expired identity. */
  get<T = unknown>(id: number): T {
    if (!this.#values.has(id)) refuse(Errors.Invalid.ValueGone);
    return this.#values.get(id) as T;
  }

  /** Release the value held by one bridge identity. */
  drop(id: number): void {
    this.#values.delete(id);
  }

  /** Check whether this adapter already retains the value. */
  has(id: number): boolean {
    return this.#values.has(id);
  }

  /** Cache a value under an identity issued by another adapter. */
  set(id: number, value: unknown): void {
    this.#values.set(id, value);
  }

  /** Release all retained values at host shutdown. */
  clear(): void {
    this.#values.clear();
  }

  /** Count the value slots retained by this adapter. */
  get size(): number {
    return this.#values.size;
  }
}

const protectedValues = new WeakSet<object>();

/** Freezes new plain-data branches once. An externally shallow-frozen value is still traversed. */
export function protect<T>(value: T): Readonly<T> {
  const visiting = new WeakSet<object>();
  /** Protect each new plain-data branch and reject cycles or accessors. */
  function visit(value: unknown): void {
    if (value === null || typeof value !== "object") {
      if (["function", "symbol"].includes(typeof value))
        refuse(Errors.Invalid.StateValue, "State must contain plain data");
      return;
    }
    if (protectedValues.has(value)) return;
    if (visiting.has(value)) refuse(Errors.Invalid.StateCycle, "State cannot contain cycles");
    const proto = Object.getPrototypeOf(value);
    if (!Array.isArray(value) && proto !== Object.prototype && proto !== null)
      refuse(Errors.Invalid.StateValue, "State requires plain objects and arrays");
    visiting.add(value);
    for (const key of Reflect.ownKeys(value)) {
      if (typeof key !== "string")
        refuse(Errors.Invalid.StateValue, "State cannot contain symbol properties");
      const descriptor = Object.getOwnPropertyDescriptor(value, key)!;
      if (!Object.hasOwn(descriptor, "value"))
        refuse(Errors.Invalid.StateValue, "State cannot contain accessors");
      visit(descriptor.value);
    }
    visiting.delete(value);
    Object.freeze(value);
    protectedValues.add(value);
  }
  visit(value);
  return value;
}
