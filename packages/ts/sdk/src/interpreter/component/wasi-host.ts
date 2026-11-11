/**
 * Shared pieces of the WASI 0.3 adapters for trusted JCO bindings: physical accounting of
 * provider work, revision retirement, and the two directions of `stream<u8>`.
 *
 * JCO hands an imported function a guest `stream<u8>` as an object with `read({ count })`
 * (resolving `{ value, done }`) and `Symbol.dispose`; it accepts any async iterable from the host
 * as a stream it produces, and any thenable as a future. A pending guest-stream read is never
 * awaited without a bound: after a guest trap JCO never settles it, so every read races the
 * revision's retirement signal, which aborts on quarantine and disposal.
 */

/** Host services an adapter uses; implemented by the component port. */
export interface WasiHost {
  /**
   * Run one provider operation under physical accounting: it belongs to the current callback
   * (or the revision between callbacks) until it settles, even when the caller stops waiting.
   * The returned promise rejects promptly when the current callback is cancelled.
   */
  physical<T>(operation: () => Promise<T>): Promise<T>;
  /** Retain cleanup/background work in its callback, or in the revision between callbacks. */
  cleanup(operation: () => unknown): void;
  /** Aborts when the revision quarantines or retires. */
  readonly retired: AbortSignal;
  /** Cancellation of the callback serving the current operation (its end, its context, retirement). */
  signal(): AbortSignal;
}

/** Import objects of one package, keyed by interface; resource classes stay private. */
export type WasiInterfaces<K extends string> = Record<K, Record<string, unknown>>;

/** WASI `result` values in JCO form. */
export type WasiResult = { tag: "ok"; val?: unknown } | { tag: "err"; val: unknown };
export const ok = (val?: unknown): WasiResult =>
  val === undefined ? { tag: "ok" } : { tag: "ok", val };
export const err = (code: unknown): WasiResult => ({ tag: "err", val: code });

export const cancelled = (): Error => new Error("Operation cancelled");

/** Reject when `signal` aborts; the operation's own settlement is unaffected. */
export function abortable<T>(operation: Promise<T>, signal: AbortSignal | undefined): Promise<T> {
  if (!signal) return operation;
  if (signal.aborted) return Promise.reject(cancelled());
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(cancelled());
    signal.addEventListener("abort", abort, { once: true });
    operation.then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

/**
 * Run a closing operation (a provider close, cancel or abort) as physical work. Started while a
 * callback is attributed, it belongs to that callback, which then completes only after it settled
 * (bounded by its deadline), so the next callback never overlaps it. Otherwise, and after
 * retirement, the revision owns it until disposal.
 */
export function closing(host: WasiHost, operation: () => unknown): void {
  host.cleanup(operation);
}

/** A JCO guest stream as seen by the host. */
interface GuestStream {
  read(options: { count: number }): Promise<{ value?: unknown; done?: boolean }>;
  [Symbol.dispose]?(): void;
}

/** One chunk of a JCO `stream<u8>` read. */
function chunk(value: unknown): Uint8Array {
  if (value instanceof Uint8Array) return value;
  if (Array.isArray(value)) return Uint8Array.from(value as number[]);
  if (ArrayBuffer.isView(value))
    return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
  if (value === undefined) return new Uint8Array(0);
  throw new TypeError("byte stream values must be bytes");
}

/** Largest chunk requested from a guest stream at once. */
export const STREAM_CHUNK = 64 * 1024;

/**
 * Consume a guest `stream<u8>` as chunks pulled on demand (backpressure). Every read is bounded
 * by `retired`; `return()` (an early stop by the consumer) drops the guest stream.
 */
export function guestBytes(
  stream: unknown,
  retired: AbortSignal,
): AsyncIterableIterator<Uint8Array> {
  const source = stream as GuestStream;
  if (typeof source?.read !== "function") throw new TypeError("expected a guest byte stream");
  let finished = false;
  const finish = () => {
    if (finished) return;
    finished = true;
    try {
      source[Symbol.dispose]?.();
    } catch {
      /* A trapped instance may refuse the drop; the stream is gone either way. */
    }
  };
  const iterator: AsyncIterableIterator<Uint8Array> = {
    [Symbol.asyncIterator]: () => iterator,
    async next() {
      while (!finished) {
        let result: { value?: unknown; done?: boolean };
        try {
          result = await abortable(source.read({ count: STREAM_CHUNK }), retired);
        } catch (error) {
          finish();
          throw error;
        }
        const bytes = chunk(result.value);
        if (bytes.byteLength) return { done: false, value: bytes };
        if (result.done) break;
      }
      finish();
      return { done: true, value: undefined };
    },
    async return() {
      finish();
      return { done: true, value: undefined };
    },
  };
  return iterator;
}

/**
 * A host-produced `stream<u8>` for JCO: chunks come from `next(limit)` (`undefined` or empty is
 * the end). `close` runs exactly once with how the stream ended. A producer failure ends the
 * stream and is reported only through `close` (whose owner settles the stream's companion
 * future with the error, as WASI streams carry no error); it is never thrown to JCO, which would
 * turn it into a guest trap.
 */
export function hostBytes(
  next: (limit: number) => Promise<Uint8Array | undefined>,
  close: (end: "eof" | "dropped" | "failed", failure?: unknown) => void,
  retired: AbortSignal,
): AsyncIterableIterator<Uint8Array> {
  let ended = false;
  const end = (how: "eof" | "dropped" | "failed", failure?: unknown) => {
    if (ended) return;
    ended = true;
    close(how, failure);
  };
  retired.addEventListener("abort", () => end("dropped"), { once: true });
  const iterator: AsyncIterableIterator<Uint8Array> = {
    [Symbol.asyncIterator]: () => iterator,
    async next() {
      if (ended) return { done: true, value: undefined };
      try {
        const bytes = await abortable(next(STREAM_CHUNK), retired);
        if (bytes && bytes.byteLength) return { done: false, value: bytes };
        end("eof");
      } catch (error) {
        end(retired.aborted ? "dropped" : "failed", error);
      }
      return { done: true, value: undefined };
    },
    async return() {
      end("dropped");
      return { done: true, value: undefined };
    },
  };
  return iterator;
}

/** A host-produced typed `stream<T>` whose items come one at a time from `next()`; failures as `hostBytes`. */
export function hostItems(
  next: () => Promise<unknown>,
  close: (end: "eof" | "dropped" | "failed", failure?: unknown) => void,
  retired: AbortSignal,
): AsyncIterableIterator<unknown> {
  let ended = false;
  const end = (how: "eof" | "dropped" | "failed", failure?: unknown) => {
    if (ended) return;
    ended = true;
    close(how, failure);
  };
  retired.addEventListener("abort", () => end("dropped"), { once: true });
  const iterator: AsyncIterableIterator<unknown> = {
    [Symbol.asyncIterator]: () => iterator,
    async next() {
      if (ended) return { done: true, value: undefined };
      try {
        const item = await abortable(next(), retired);
        if (item !== undefined) return { done: false, value: item };
        end("eof");
      } catch (error) {
        end(retired.aborted ? "dropped" : "failed", error);
      }
      return { done: true, value: undefined };
    },
    async return() {
      end("dropped");
      return { done: true, value: undefined };
    },
  };
  return iterator;
}
