import type { Invocation, Snapshot } from "../bridge/kernel.ts";

export interface Readable<T> {
  readonly value: Readonly<T>;

  /** Derive a cached readable value from this source. */
  select<R>(compute: (value: Readonly<T>) => R): Readable<R>;

  /** Deliver the current value, then changes, until detached. */
  subscribe(listener: (value: Readonly<T>) => void): () => void;
}

export interface StateOptions<T> {
  version?: number;
  validate?: (candidate: Readonly<T>) => boolean | void;
  migrate?: (previous: unknown, fromVersion: number) => T;
}

export interface StateBackend {
  /** Read value identity and revision from the kernel. */
  snapshot(object: number): Snapshot;

  /** Resolve an adapter-owned value identity. */
  value<T>(id: number): T;

  /** Adopt state only under the expected revision and admitted call. */
  adopt(object: number, context: unknown, expected: string, candidate: unknown): Snapshot;

  /** Require a live call that owns this object. */
  check(context: unknown, object: number): Invocation;

  /** Report observer failures without reverting adopted state. */
  report(error: unknown): void;

  /** Queue notifications in adoption order. */
  publish(operation: () => void): void;
}

export interface State<T> extends Readable<T> {
  /** Validate and adopt a synchronous candidate under the supplied context. */
  update(context: unknown, change: (current: Readonly<T>) => T): Readonly<T>;
}

export interface PreparedSelection {
  /** Publish the staged value and return its notification callback. */
  adopt(): () => void;
}
