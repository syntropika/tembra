/** Callback-owned producers for standard WASI stream and future parameters. */
import { TYPES, WRITABLE_FUTURES } from "../../../../packages/ts/sdk/src/interpreter/standard.ts";
import {
  DROP,
  FUTURE_NEW,
  Reader,
  STANDARD_PACKET,
  STREAM_CHUNK,
  STREAM_NEW,
  STREAM_WRITE,
  Writer,
} from "../../../../packages/ts/sdk/src/interpreter/standard-codec.ts";

const dispose: symbol = (Symbol as any).dispose ?? Symbol.for("Symbol.dispose");
const READ_CHUNK = Symbol.for("tembra.wasi.stream.read-chunk");
const WRITE_CHUNKS = Symbol.for("tembra.wasi.stream.write-chunks");

function packet(operation: number): Writer {
  const writer = new Writer();
  writer.u8(STANDARD_PACKET);
  writer.u8(operation);
  return writer;
}

interface Input {
  readonly handle: number;
  close(): void;
}

/** One revision's callback-owned writers; the runtime closes them at callback end. */
export class InputBindings {
  readonly active = new Set<Input>();
  /** Body pumps still producing; futures depend on their author's promise instead. */
  readonly pumps = new Set<Input>();
  #idle: (() => void)[] = [];
  constructor(
    readonly sync: (bytes: Uint8Array) => Uint8Array,
    readonly async: (bytes: Uint8Array) => Promise<Uint8Array>,
    readonly writeFuture: (handle: number, payload: number, value: unknown) => void,
    readonly discardFuture: (payload: number, value: unknown) => void,
  ) {}

  call(): InputCall {
    return new InputCall(this);
  }

  close(): void {
    for (const writer of [...this.active]) writer.close();
  }

  /** A writer finished or closed; wake `idle` waiters once no pump is left. */
  finished(input: Input): void {
    this.active.delete(input);
    this.pumps.delete(input);
    if (this.pumps.size) return;
    const waiters = this.#idle;
    this.#idle = [];
    for (const wake of waiters) wake();
  }

  /** Resolve once every body pump of the current callback has finished or closed. */
  idle(): Promise<void> {
    return this.pumps.size
      ? new Promise<void>((resolve) => this.#idle.push(resolve))
      : Promise.resolve();
  }

  drop(handle: number): void {
    const writer = packet(DROP);
    writer.handle(handle);
    this.sync(writer.finish());
  }

  create(operation: number, type: number): number {
    const writer = packet(operation);
    writer.u32(type);
    const reader = new Reader(this.sync(writer.finish()));
    const handle = reader.handle();
    reader.end();
    return handle;
  }
}

/** Writers allocated by one call are rolled back together if argument validation fails. */
class InputCall {
  readonly #pumps: Pump[] = [];
  readonly #futures: FutureInput[] = [];
  constructor(readonly bindings: InputBindings) {}

  handle(kind: string, detail: unknown, source: any): number {
    if (kind === "stream") {
      if (
        !(source instanceof Uint8Array) &&
        typeof source?.[WRITE_CHUNKS] !== "function" &&
        typeof source?.[READ_CHUNK] !== "function" &&
        typeof source?.[Symbol.asyncIterator] !== "function" &&
        typeof source?.[Symbol.iterator] !== "function"
      )
        throw new TypeError("stream<u8> arguments require an AsyncIterable<number>");
      const type = TYPES.findIndex(([kind, item]) => kind === "stream" && item === detail);
      if (type < 0) throw new TypeError("unsupported input stream type");
      const handle = this.bindings.create(STREAM_NEW, type);
      try {
        const input = new Pump(this.bindings, handle, source);
        this.#pumps.push(input);
        return handle;
      } catch (error) {
        this.bindings.drop(handle);
        throw error;
      }
    }
    const type = TYPES.findIndex(([kind, payload]) => kind === "future" && payload === detail);
    if (!WRITABLE_FUTURES.includes(type)) throw new TypeError("unsupported input future type");
    if (typeof source?.then !== "function")
      throw new TypeError("future arguments require a PromiseLike");
    const input = new FutureInput(
      this.bindings,
      this.bindings.create(FUTURE_NEW, type),
      detail as number,
      source,
    );
    this.#futures.push(input);
    return input.handle;
  }

  start(): Promise<unknown>[] {
    const running = this.#pumps.map((pump) => pump.run());
    // A successful trailer must follow successful production. Sending it before a throwing
    // body producer finished would allow the host to commit a truncated HTTP upload.
    for (const future of this.#futures) future.start(running);
    return running;
  }

  close(): void {
    for (const input of [...this.#pumps, ...this.#futures]) input.close();
  }
}

/** Rejected or abandoned HTTP input futures report a standard error to their consumer. */
function failed(error: unknown) {
  return { tag: "err", val: { tag: "internal-error", val: String(error) } };
}

class FutureInput implements Input {
  #closed = false;
  readonly #value: Promise<unknown>;
  #resolved: unknown;
  constructor(
    readonly bindings: InputBindings,
    readonly handle: number,
    readonly payload: number,
    source: PromiseLike<unknown>,
  ) {
    // Install the rejection handler immediately, including while body production is pending.
    this.#value = Promise.resolve(source)
      .catch(failed)
      .then((value) => {
        this.#resolved = value;
        if (this.#closed) bindings.discardFuture(payload, value);
        return value;
      });
    bindings.active.add(this);
  }

  start(pumps: Promise<unknown>[]): void {
    void Promise.all([this.#value, Promise.all(pumps)]).then(([value, failures]) => {
      if (this.#closed) return;
      const failure = failures.find((failure) => failure !== undefined);
      try {
        this.bindings.writeFuture(
          this.handle,
          this.payload,
          failure === undefined ? value : failed(failure),
        );
      } catch {
        // Invalid values leave the writer untouched. DROP resolves it as a standard error.
      } finally {
        this.close();
      }
    });
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.bindings.finished(this);
    this.bindings.discardFuture(this.payload, this.#resolved);
    try {
      this.bindings.drop(this.handle);
    } catch {
      // The host closes unwritten writers when the callback has already ended.
    }
  }
}

/** A failed STREAM_WRITE: the consumer is gone and its future reports why. */
class WriteFailure extends Error {}

/** Pump one guest iterable into a host input stream, one bounded chunk at a time. */
class Pump {
  #closed = false;
  #iterator: Iterator<unknown> | AsyncIterator<unknown> | undefined;
  #chunked = true;

  constructor(
    readonly bindings: InputBindings,
    readonly handle: number,
    source: any,
  ) {
    if (source instanceof Uint8Array) {
      const chunks = [source];
      this.#iterator = chunks[Symbol.iterator]();
    } else if (typeof source?.[WRITE_CHUNKS] === "function")
      this.#iterator = source[WRITE_CHUNKS]();
    else if (typeof source?.[READ_CHUNK] === "function")
      this.#iterator = {
        next: async () => {
          try {
            const value = await source[READ_CHUNK]();
            return value ? { done: false, value } : { done: true, value: undefined };
          } catch (error) {
            source[dispose]?.();
            throw error;
          }
        },
        return: async () => {
          source[dispose]?.();
          return { done: true, value: undefined };
        },
      };
    else {
      this.#chunked = false;
      this.#iterator = source[Symbol.asyncIterator]?.() ?? source[Symbol.iterator]();
    }
    this.bindings.active.add(this);
    this.bindings.pumps.add(this);
  }

  /** Run to end of input; resolve with the source's own failure, if any. */
  async run(): Promise<unknown> {
    let failure: unknown;
    try {
      const batch = new Uint8Array(this.#chunked ? 0 : STREAM_CHUNK);
      let length = 0;
      for (;;) {
        let step: IteratorResult<unknown>;
        try {
          step = await this.#iterator!.next();
        } catch (error) {
          // A throwing iterator is finished; it is not returned.
          this.#iterator = undefined;
          throw error;
        }
        if (this.#closed) return undefined;
        if (step.done) break;
        if (this.#chunked) {
          const chunk = step.value;
          if (!(chunk instanceof Uint8Array))
            throw new TypeError("stream<u8> chunk sources must yield Uint8Array");
          for (let offset = 0; offset < chunk.length; offset += STREAM_CHUNK)
            await this.#write(chunk.subarray(offset, offset + STREAM_CHUNK));
        } else {
          const byte = step.value;
          if (typeof byte !== "number" || !Number.isInteger(byte) || byte < 0 || byte > 255)
            throw new TypeError("stream<u8> items must be integers in 0..=255");
          batch[length++] = byte;
          if (length === STREAM_CHUNK) {
            await this.#write(batch);
            length = 0;
          }
        }
        if (this.#closed) return undefined;
      }
      this.#iterator = undefined;
      if (length) await this.#write(batch.subarray(0, length));
    } catch (error) {
      if (!(error instanceof WriteFailure)) failure = error;
    } finally {
      this.close();
    }
    return failure;
  }

  /** Write one chunk; the completion is the host's backpressure. */
  async #write(chunk: Uint8Array): Promise<void> {
    if (this.#closed) throw new WriteFailure("stream was closed");
    const writer = packet(STREAM_WRITE);
    writer.handle(this.handle);
    writer.bytes(chunk);
    try {
      await this.bindings.async(writer.finish());
    } catch (error) {
      throw new WriteFailure(String(error));
    }
  }

  /** Return an unfinished iterator and drop the writer: the input reaches end of stream. */
  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.bindings.finished(this);
    const iterator = this.#iterator;
    this.#iterator = undefined;
    try {
      const returned: any = iterator?.return?.();
      if (typeof returned?.then === "function") returned.then(undefined, () => {});
    } catch {
      // The source's cleanup failure cannot reopen the stream.
    }
    try {
      this.bindings.drop(this.handle);
    } catch {
      // Outside an active scope the host closes the writer at callback end.
    }
  }
}
