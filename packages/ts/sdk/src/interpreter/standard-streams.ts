/**
 * Generic ownership pieces of the browser standard WASI host: the services an implementation
 * may use, revision-owned resources, one-shot futures, host-to-guest output streams and
 * guest-to-host input streams. Nothing here knows a WASI interface; method implementations
 * (`standard-filesystem.ts`, `standard-system.ts`) compose these pieces.
 */
import { STREAM_CHUNK, Writer } from "./standard-codec.ts";

/** Host services available to a method implementation. */
export interface StandardServices {
  readonly packetBytes: number;
  /** Whether the revision is retired; physical work is then refused. */
  readonly closed: boolean;
  /** Run an uncancellable provider operation under the revision's physical accounting. */
  physical<T>(signal: AbortSignal | undefined, operation: () => Promise<T>): Promise<T>;
  /** Track a destructor-like operation even while the revision is retiring. */
  cleanup(operation: () => unknown): void;
}

/** One decoded call: the host services plus the owning callback's cancellation signal. */
export interface StandardCall {
  readonly host: StandardServices;
  readonly signal?: AbortSignal | undefined;
}

/** One method implementation; it receives decoded jco-convention arguments. */
export type Implementation = (call: StandardCall, args: any) => unknown;

/** A host value behind an `own`/`borrow` handle; streams and calls retain it independently. */
export interface StandardResource {
  /** Pending asynchronous calls that borrowed this resource. */
  borrows: number;
  retain(): void;
  /** Release one user; the last release closes the underlying value. */
  release(): void;
}

export const cancelled = (): Error => new Error("Operation cancelled");

/** Reject when `signal` aborts, without abandoning the physical operation's accounting. */
export function abortable<T>(operation: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) return operation;
  if (signal.aborted) return Promise.reject(cancelled());
  return new Promise<T>((resolve, reject) => {
    const abort = () => reject(cancelled());
    signal.addEventListener("abort", abort, { once: true });
    operation.then(resolve, reject).finally(() => signal.removeEventListener("abort", abort));
  });
}

/** A one-shot WIT future. `cancel` runs when the reply that would publish it is never delivered. */
export class HostFuture<T = unknown> {
  constructor(
    readonly value: Promise<T>,
    readonly cancel?: () => void,
  ) {}

  /** A future whose payload is already known. */
  static of<T>(value: T): HostFuture<T> {
    return new HostFuture(Promise.resolve(value));
  }
}

/** Producer of a `stream<u8>` output. `close` runs once, at end of stream or on drop. */
export interface ByteProducer {
  /** At most `limit` bytes; `undefined` is end of stream. A failure ends the stream. */
  next(limit: number): Promise<Uint8Array | undefined>;
  close(): void;
}

/** Producer of a typed `stream<T>` output, one item at a time. */
export interface ItemProducer {
  /** The next item, or `undefined` at end of stream. */
  next(): Promise<unknown>;
  close(): void;
}

/**
 * A host-to-guest stream. Reads are serialized; a read whose callback was cancelled keeps its
 * produced data for the next read instead of losing it. A pending read fails immediately when
 * the stream is dropped, while the producer's physical call stays tracked by its producer.
 */
export class OutputStream {
  #queue: Promise<unknown> = Promise.resolve();
  readonly #dropped = new AbortController();
  #ended = false;
  #closed = false;
  #abandoned = false;
  /** Produced but undelivered bytes, or encoded items. */
  #stash: Uint8Array[] = [];
  /** The producer call whose reader was cancelled; the next read takes its result. */
  #inflight: Promise<Uint8Array | undefined> | undefined;

  /**
   * @param producer a byte producer for `stream<u8>`, an item producer for typed streams
   * @param settle reports end of stream (`undefined`) or the producer's failure to the future
   */
  constructor(
    readonly producer: ByteProducer | ItemProducer,
    readonly settle: (failure?: unknown) => void,
  ) {}

  /** End once: settle the paired future and release the producer. */
  #end(failure?: unknown): void {
    if (this.#ended) return;
    this.#ended = true;
    this.settle(failure);
    this.producer.close();
  }

  /** Drop by the guest or revision retirement. */
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#dropped.abort();
    this.#stash = [];
    this.#inflight = undefined;
    this.#end();
  }

  /** A produced read never reached its guest; later reads must not silently skip its data. */
  abandon(): void {
    this.#abandoned = true;
    this.#end(new Error("stream read was abandoned"));
    this.close();
  }

  /**
   * Serve one STREAM_READ: the full reply (`u8 0` + chunk or `u8 1`). `limit` is a byte bound
   * for byte streams and an item bound for typed streams; `bound` is the reply byte bound.
   * `encodeItem`, supplied by the host from the stream's generated item type, selects the
   * typed batch layout.
   */
  read(
    limit: number,
    bound: number,
    signal?: AbortSignal,
    encodeItem?: (item: unknown) => Uint8Array,
  ): Promise<Uint8Array> {
    const next = this.#queue.then(() => this.#serve(limit, bound, signal, encodeItem));
    this.#queue = next.then(
      () => {},
      () => {},
    );
    return next;
  }

  async #serve(
    limit: number,
    bound: number,
    signal?: AbortSignal,
    encodeItem?: (item: unknown) => Uint8Array,
  ): Promise<Uint8Array> {
    const writer = new Writer();
    if (this.#abandoned) throw new Error("stream read was abandoned");
    if (this.#closed) throw new Error("stream was dropped");
    if (signal?.aborted) throw cancelled();
    if (!encodeItem) {
      const chunk = this.#stash.shift() ?? (await this.#produce(limit, signal));
      if (!chunk) {
        writer.u8(1);
        return writer.finish();
      }
      if (chunk.byteLength > limit) this.#stash.unshift(chunk.subarray(limit));
      writer.u8(0);
      writer.bytes(chunk.subarray(0, limit));
      return writer.finish();
    }
    const items: Uint8Array[] = [];
    let size = 5;
    try {
      while (items.length < limit) {
        const item = this.#stash.shift() ?? (await this.#produce(1, signal, encodeItem));
        if (!item) break;
        if (size + item.byteLength > bound) {
          if (!items.length) {
            this.#end(new Error("stream item exceeds the packet bound"));
            throw new Error("stream item exceeds the packet bound");
          }
          this.#stash.unshift(item);
          break;
        }
        items.push(item);
        size += item.byteLength;
      }
    } catch (error) {
      // No reply exists yet. Preserve the already assembled prefix for the next callback.
      if (!this.#closed) this.#stash.unshift(...items);
      throw error;
    }
    if (!items.length) {
      writer.u8(1);
      return writer.finish();
    }
    writer.u8(0);
    writer.u32(items.length);
    for (const item of items) writer.raw(item);
    return writer.finish();
  }

  /**
   * Produce one chunk or encoded item. Production is never concurrent: a read cancelled while
   * its producer call runs leaves that call in flight, and the next read takes its result.
   */
  async #produce(
    limit: number,
    signal?: AbortSignal,
    encodeItem?: (item: unknown) => Uint8Array,
  ): Promise<Uint8Array | undefined> {
    let inflight = this.#inflight;
    if (!inflight) {
      if (this.#ended) return undefined;
      inflight = this.#inflight = (async () => {
        try {
          if (!encodeItem) {
            const chunk = await (this.producer as ByteProducer).next(limit);
            if (chunk?.byteLength) return chunk;
            this.#end();
            return undefined;
          }
          const item = await (this.producer as ItemProducer).next();
          if (item !== undefined) return encodeItem(item);
          this.#end();
          return undefined;
        } catch (error) {
          this.#end(error);
          return undefined;
        }
      })();
    }
    try {
      const value = await abortable(abortable(inflight, this.#dropped.signal), signal);
      this.#inflight = undefined;
      return value;
    } catch (error) {
      if (this.#abandoned) throw new Error("stream read was abandoned");
      if (this.#closed) throw new Error("stream was dropped");
      throw error;
    }
  }
}

/** Accepted input bytes per revision across all writers, matching the native budget. */
export const INPUT_BUDGET = 1 << 20;

/**
 * The revision's aggregate budget of accepted guest input. A chunk is reserved before it is
 * copied out of its packet and stays reserved while it is buffered or waiting for room in its
 * stream; it is released when its consumer takes it or the stream discards it. Waiters are
 * served in order, so a large chunk is not starved by smaller ones.
 */
export class InputBudget {
  #used = 0;
  readonly #waiters: { bytes: number; grant(): void }[] = [];

  constructor(readonly limit = INPUT_BUDGET) {}

  /** Reserved bytes; never above `limit`. */
  get used(): number {
    return this.#used;
  }

  /**
   * Reserve `bytes`, waiting for capacity. `signal` (the callback) rejects with a cancellation;
   * `interrupt` (the stream) rejects with its own reason. A rejected wait reserves nothing.
   */
  reserve(bytes: number, signal?: AbortSignal, interrupt?: AbortSignal): Promise<void> {
    if (signal?.aborted) return Promise.reject(cancelled());
    if (interrupt?.aborted) return Promise.reject(interrupt.reason);
    if (!this.#waiters.length && this.#used + bytes <= this.limit) {
      this.#used += bytes;
      return Promise.resolve();
    }
    return new Promise<void>((resolve, reject) => {
      const detach = () => {
        signal?.removeEventListener("abort", onSignal);
        interrupt?.removeEventListener("abort", onInterrupt);
      };
      const waiter = {
        bytes,
        grant: () => {
          detach();
          resolve();
        },
      };
      const leave = (error: unknown) => {
        const at = this.#waiters.indexOf(waiter);
        if (at < 0) return;
        this.#waiters.splice(at, 1);
        detach();
        reject(error);
        // A removed head may unblock smaller waiters behind it.
        this.#drain();
      };
      const onSignal = () => leave(cancelled());
      const onInterrupt = () => leave(interrupt!.reason);
      signal?.addEventListener("abort", onSignal, { once: true });
      interrupt?.addEventListener("abort", onInterrupt, { once: true });
      this.#waiters.push(waiter);
    });
  }

  release(bytes: number): void {
    this.#used -= bytes;
    this.#drain();
  }

  #drain(): void {
    while (this.#waiters.length && this.#used + this.#waiters[0]!.bytes <= this.limit) {
      const waiter = this.#waiters.shift()!;
      this.#used += waiter.bytes;
      waiter.grant();
    }
  }
}

/**
 * A guest-to-host `stream<u8>`: one bounded buffer of at most `STREAM_CHUNK` bytes, written by
 * the guest through STREAM_WRITE and consumed by the method it was transferred to. Every
 * buffered or pending byte holds a reservation in the revision's `InputBudget`.
 */
export class InputStream {
  /** Set once the readable end was passed to a successful call. */
  transferred = false;
  #chunks: Uint8Array[] = [];
  #buffered = 0;
  #eof = false;
  #cancelled = false;
  /** The write in progress (waiting for budget or for room); close/cancel interrupt it. */
  #writing: AbortController | undefined;
  #pending: { chunk: Uint8Array; resolve(): void } | undefined;
  #wake: (() => void) | undefined;

  /** @param type the generated `TYPES` index of this stream */
  constructor(
    readonly type: number,
    readonly budget: InputBudget,
  ) {}

  /** Bytes accepted but not yet consumed; never above `STREAM_CHUNK`. */
  get buffered(): number {
    return this.#buffered;
  }

  /** Whether the consumer stopped. */
  get cancelled(): boolean {
    return this.#cancelled;
  }

  #notify(): void {
    const wake = this.#wake;
    this.#wake = undefined;
    wake?.();
  }

  #accept(chunk: Uint8Array): void {
    if (!chunk.byteLength) return;
    this.#chunks.push(chunk);
    this.#buffered += chunk.byteLength;
    this.#notify();
  }

  /**
   * Accept one whole chunk of `length` bytes. State is checked first; then the chunk is
   * reserved in the aggregate budget, and only then copied with `copy`. One write may be in
   * progress per stream; it completes once the chunk fits this stream's buffer.
   */
  async write(length: number, copy: () => Uint8Array, signal?: AbortSignal): Promise<void> {
    if (length > STREAM_CHUNK) throw new Error("stream write exceeds the 64 KiB chunk bound");
    if (this.#eof) throw new Error("stream was dropped");
    if (this.#cancelled) throw new Error("stream reader was dropped");
    if (this.#writing) throw new Error("a stream write is already pending");
    if (signal?.aborted) throw cancelled();
    const writing = (this.#writing = new AbortController());
    try {
      await this.budget.reserve(length, signal, writing.signal);
      // Either side may have stopped while the grant was being delivered.
      if (writing.signal.aborted || signal?.aborted) {
        this.budget.release(length);
        throw writing.signal.aborted ? writing.signal.reason : cancelled();
      }
      const chunk = copy();
      if (this.#buffered + length <= STREAM_CHUNK) return this.#accept(chunk);
      await new Promise<void>((resolve, reject) => {
        const stop = (error: unknown) => {
          if (this.#pending?.chunk !== chunk) return;
          this.#pending = undefined;
          this.budget.release(length);
          signal?.removeEventListener("abort", onSignal);
          reject(error);
        };
        const onSignal = () => stop(cancelled());
        signal?.addEventListener("abort", onSignal, { once: true });
        writing.signal.addEventListener("abort", () => stop(writing.signal.reason), {
          once: true,
        });
        this.#pending = {
          chunk,
          resolve: () => {
            signal?.removeEventListener("abort", onSignal);
            resolve();
          },
        };
      });
    } finally {
      if (this.#writing === writing) this.#writing = undefined;
    }
  }

  /** Writer drop: end of stream after buffered bytes; a pending chunk is not delivered. */
  close(): void {
    if (this.#eof) return;
    this.#eof = true;
    this.#writing?.abort(new Error("stream was dropped"));
    if (!this.transferred) this.#discard();
    this.#notify();
  }

  /** The consumer stopped: later writes fail and buffered bytes are discarded. */
  cancel(): void {
    if (this.#cancelled) return;
    this.#cancelled = true;
    this.#writing?.abort(new Error("stream reader was dropped"));
    this.#discard();
    this.#notify();
  }

  #discard(): void {
    this.budget.release(this.#buffered);
    this.#chunks = [];
    this.#buffered = 0;
  }

  /** Next buffered bytes (at most `STREAM_CHUNK`), or `undefined` at end of stream. */
  async read(): Promise<Uint8Array | undefined> {
    while (!this.#buffered) {
      if (this.#eof || this.#cancelled) return undefined;
      await new Promise<void>((resolve) => {
        this.#wake = resolve;
      });
    }
    const chunks = this.#chunks;
    const total = this.#buffered;
    this.#chunks = [];
    this.#buffered = 0;
    // The consumer owns these bytes now; their reservation returns to the revision.
    this.budget.release(total);
    let bytes = chunks[0]!;
    if (chunks.length > 1) {
      bytes = new Uint8Array(total);
      let offset = 0;
      for (const chunk of chunks) {
        bytes.set(chunk, offset);
        offset += chunk.byteLength;
      }
    }
    // The emptied buffer admits the pending write whole, as the writer was promised.
    const pending = this.#pending;
    if (pending) {
      this.#pending = undefined;
      this.#accept(pending.chunk);
      pending.resolve();
    }
    return bytes;
  }
}
