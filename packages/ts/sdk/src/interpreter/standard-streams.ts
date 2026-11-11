/**
 * Generic ownership pieces of the browser standard WASI host: the services an implementation
 * may use, revision-owned resources, one-shot futures, host-to-guest output streams and
 * guest-to-host input streams. Nothing here knows a WASI interface; method implementations
 * (`standard-filesystem.ts`, `standard-system.ts`) compose these pieces.
 */
import { METHODS, RESOURCES, type WasiMethod } from "./standard.ts";
import { STREAM_CHUNK, Writer } from "./standard-codec.ts";

/**
 * Implementation key of a generated method: resource functions are qualified by their resource
 * (`<package>/<interface>#<resource>.<name>`), so `new`, `clone` or `consume-body` of different
 * resources never collide. Free functions use `<interface>#<name>`.
 */
export function methodKey(method: WasiMethod): string {
  return method.resource === undefined
    ? `${method.interface}#${method.name}`
    : `${RESOURCES[method.resource]}.${method.name}`;
}

/** A WIT constructor or static function: the call, then its decoded parameters. */
export type StaticImplementation = (call: StandardCall, ...args: any[]) => unknown;

/**
 * Bind every generated function of one resource: constructors and statics to `statics` by
 * generated name, methods to the same-named method of the borrowed `self`, which receives the
 * decoded parameters followed by the call. A generated function without an implementation
 * fails construction instead of a later call.
 */
export function bindResource(
  contract: string,
  resource: string,
  prototype: object,
  statics: Readonly<Record<string, StaticImplementation>>,
): Record<string, Implementation> {
  const implementations: Record<string, Implementation> = {};
  const qualified = `${contract}#${resource}`;
  for (const method of METHODS) {
    if (method.resource === undefined || RESOURCES[method.resource] !== qualified) continue;
    const name = method.name;
    if (method.kind !== undefined) {
      if (!Object.hasOwn(statics, name)) throw new Error(`${qualified}.${name} is unbound`);
      const implementation = statics[name]!;
      implementations[methodKey(method)] = (call, args: unknown[]) => implementation(call, ...args);
    } else {
      if (typeof (prototype as Record<string, unknown>)[name] !== "function")
        throw new Error(`${qualified}.${name} is unbound`);
      implementations[methodKey(method)] = (call, [self, ...args]: unknown[]) =>
        (self as Record<string, (...values: unknown[]) => unknown>)[name]!(...args, call);
    }
  }
  return implementations;
}

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

/**
 * A one-shot host-to-guest WIT future. `cancel` runs when the reply that would publish it, or
 * the read that would deliver its value, can never reach the guest. A guest dropping the handle
 * does not cancel the work behind it (as upstream `write-via-stream` results do not); the host
 * only releases the value's owned resources when it arrives.
 */
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

/** The readable end of a guest-written future, held by the call it was passed to. */
export interface FutureReader<T = unknown> {
  /** The value once written; ownership of any resources inside it moves to the caller. */
  take(): Promise<T>;
  /** The consumer stopped: a written or later value is released, never delivered. */
  close(): void;
}

/**
 * A guest-to-host one-shot future created by FUTURE_NEW. One handle names its writer end
 * (FUTURE_WRITE, DROP) and, until it is passed once as a call parameter, its readable end. A
 * writer that ends unwritten (dropped, or its callback ended) resolves to `abandoned`, so a
 * consumer never mistakes a vanished producer for a completed one.
 */
export class WritableFuture<T = unknown> implements FutureReader<T> {
  /** Set once the readable end was passed to a successful call. */
  transferred = false;
  #written = false;
  #taken = false;
  #closed = false;
  #value: { readonly value: T } | undefined;
  #deliver: ((value: T) => void) | undefined;

  /**
   * @param type the generated `TYPES` index of this future
   * @param abandoned the value an unwritten writer resolves to; it carries no resources
   * @param release releases the resources inside a value nobody will take
   */
  constructor(
    readonly type: number,
    readonly abandoned: T,
    readonly release: (value: T) => void,
  ) {}

  /** Whether the writer end has ended (written or abandoned). */
  get written(): boolean {
    return this.#written;
  }

  /** Accept the one value; the caller has already claimed every resource inside it. */
  write(value: T): void {
    if (this.#written) throw new Error("future was already written");
    this.#written = true;
    this.#settle(value);
  }

  /** The writer ended without a value. */
  abandon(): void {
    if (this.#written) return;
    this.#written = true;
    this.#settle(this.abandoned);
  }

  #settle(value: T): void {
    if (this.#closed) return this.release(value);
    if (this.#deliver) {
      const deliver = this.#deliver;
      this.#deliver = undefined;
      return deliver(value);
    }
    this.#value = { value };
  }

  take(): Promise<T> {
    if (this.#taken || this.#closed) return Promise.reject(new Error("future was already read"));
    this.#taken = true;
    const stored = this.#value;
    this.#value = undefined;
    if (stored) return Promise.resolve(stored.value);
    return new Promise<T>((resolve) => {
      this.#deliver = resolve;
    });
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#deliver = undefined;
    const stored = this.#value;
    this.#value = undefined;
    if (stored) this.release(stored.value);
  }
}

/**
 * How a host-to-guest stream ended: its producer reached the end (`eof`), the guest or the
 * retiring revision dropped it first (`dropped`), or production failed (`failed`).
 */
export type StreamEnd = "eof" | "dropped" | "failed";

/** Producer of a `stream<u8>` output. `close` runs once, at the end with its reason. */
export interface ByteProducer {
  /**
   * At most `limit` bytes. `undefined` or an empty chunk is end of stream, so a producer whose
   * source may yield empty chunks must skip them. A failure ends the stream.
   */
  next(limit: number): Promise<Uint8Array | undefined>;
  close(end?: StreamEnd): void;
}

/** Producer of a typed `stream<T>` output, one item at a time. */
export interface ItemProducer {
  /** The next item, or `undefined` at end of stream. */
  next(): Promise<unknown>;
  close(end?: StreamEnd): void;
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
   * Owned-item mode: the one producer call in flight, or its result held for the next read. It
   * belongs to the stream (the revision), not to the read that started it.
   */
  #pending: Promise<{ value: unknown } | undefined> | undefined;

  /**
   * @param producer a byte producer for `stream<u8>`, an item producer for typed streams
   * @param settle reports how the stream ended, with the producer's failure for `failed`
   * @param release owned-item streams only: releases a produced value no read will deliver
   */
  constructor(
    readonly producer: ByteProducer | ItemProducer,
    readonly settle: (failure: unknown, end: StreamEnd) => void,
    readonly release?: (value: unknown) => void,
  ) {}

  /** End once: settle the paired future and release the producer, both told why. */
  #end(end: StreamEnd, failure?: unknown): void {
    if (this.#ended) return;
    this.#ended = true;
    this.settle(failure, end);
    this.producer.close(end);
  }

  /** Drop by the guest or revision retirement. */
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#dropped.abort();
    this.#stash = [];
    this.#inflight = undefined;
    // A held or late owned value has no reader any more; it is released when it exists.
    const pending = this.#pending;
    this.#pending = undefined;
    pending?.then(
      (taken) => taken && this.release?.(taken.value),
      () => {},
    );
    this.#end("dropped");
  }

  /** Whether the guest dropped the stream or the revision retired it. */
  get closed(): boolean {
    return this.#closed;
  }

  /** A produced read never reached its guest; later reads must not silently skip its data. */
  abandon(): void {
    this.#abandoned = true;
    this.#end("failed", new Error("stream read was abandoned"));
    this.close();
  }

  /**
   * Owned-item mode: serve one STREAM_READ with exactly one produced value, or `undefined` at
   * end of stream. The value is not encoded and owns no handle yet: the host encodes and issues
   * it in the reply. A read cancelled while the producer call runs leaves that call to the
   * stream, and the next read takes its value, so a value is neither lost nor taken twice.
   */
  take(signal?: AbortSignal): Promise<{ value: unknown } | undefined> {
    const next = this.#queue.then(async () => {
      if (this.#abandoned) throw new Error("stream read was abandoned");
      if (this.#closed) throw new Error("stream was dropped");
      if (signal?.aborted) throw cancelled();
      if (!this.#pending) {
        if (this.#ended) return undefined;
        this.#pending = (async () => {
          try {
            const value = await (this.producer as ItemProducer).next();
            if (value !== undefined) return { value };
            this.#end("eof");
          } catch (error) {
            this.#end("failed", error);
          }
          return undefined;
        })();
      }
      const pending = this.#pending;
      try {
        const taken = await abortable(abortable(pending, this.#dropped.signal), signal);
        // `close()` may run after the value resolved but before this continuation. It then
        // captured `#pending` and owns the value's release, so this read must not deliver it.
        if (this.#abandoned) throw new Error("stream read was abandoned");
        if (this.#closed) throw new Error("stream was dropped");
        this.#pending = undefined;
        return taken;
      } catch (error) {
        if (this.#abandoned) throw new Error("stream read was abandoned");
        if (this.#closed) throw new Error("stream was dropped");
        throw error;
      }
    });
    this.#queue = next.then(
      () => {},
      () => {},
    );
    return next;
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
            this.#end("failed", new Error("stream item exceeds the packet bound"));
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
            this.#end("eof");
            return undefined;
          }
          const item = await (this.producer as ItemProducer).next();
          if (item !== undefined) return encodeItem(item);
          this.#end("eof");
          return undefined;
        } catch (error) {
          this.#end("failed", error);
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
  /** Streams that hold, or may still take, reservations of this budget. */
  readonly #streams = new Set<InputStream>();
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

  /** Account `stream` until it finished; `discard` reaches it until then. */
  attach(stream: InputStream): void {
    this.#streams.add(stream);
  }

  detach(stream: InputStream): void {
    this.#streams.delete(stream);
  }

  /**
   * Revision retirement, after every consumer had its turn: cancel each stream that still
   * holds bytes for a consumer that will never come, so the budget returns to zero.
   */
  discard(): void {
    for (const stream of [...this.#streams]) stream.cancel();
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
  readonly #stopped = new AbortController();

  /** @param type the generated `TYPES` index of this stream */
  constructor(
    readonly type: number,
    readonly budget: InputBudget,
  ) {
    budget.attach(this);
  }

  /** Aborts when the consumer stopped (`cancel`), including revision retirement. */
  get stopped(): AbortSignal {
    return this.#stopped.signal;
  }

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
    if (!this.transferred) {
      this.#discard();
      this.budget.detach(this);
    }
    this.#notify();
  }

  /** The consumer stopped: later writes fail and buffered bytes are discarded. */
  cancel(): void {
    if (this.#cancelled) return;
    this.#cancelled = true;
    this.#writing?.abort(new Error("stream reader was dropped"));
    this.#discard();
    this.budget.detach(this);
    this.#stopped.abort();
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
      if (this.#eof || this.#cancelled) {
        this.budget.detach(this);
        return undefined;
      }
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
