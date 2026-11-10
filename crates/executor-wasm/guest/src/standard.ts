// Standard WASI bindings interpreted from the table generated from WASI WIT at build time.
// The guest owns only handle numbers; the host validates every handle before using it.
import { METHODS, RESOURCES, TYPES } from "../../../../packages/ts/sdk/src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  FUTURE_READ,
  type HandleCodec,
  Reader,
  STANDARD_PACKET,
  STREAM_CHUNK,
  STREAM_NEW,
  STREAM_READ,
  STREAM_WRITE,
  Writer,
  decode,
  encode,
} from "../../../../packages/ts/sdk/src/interpreter/standard-codec.ts";

(() => {
  const syncBytes = globalThis.__hostSyncBytes!;
  delete globalThis.__hostSyncBytes;
  const imported = new Set(
    globalThis.__tembraContract.imports
      .filter((item) => item.type_only === false)
      .map((item) => item.contract),
  );
  interface Owner {
    readonly handle: number;
    readonly resource: number;
    borrows: number;
    released: boolean;
  }
  const owners = new WeakMap<object, Owner>();
  const collected = new Set<Owner>();
  // Collection only queues identities. Host I/O runs at a safe callback boundary, never from
  // a GC callback, and a pending borrow keeps its descriptor live until that call settles.
  const finalizers = new FinalizationRegistry<Owner>((owner) => collected.add(owner));
  const dispose: symbol = (Symbol as any).dispose ?? Symbol.for("Symbol.dispose");
  /** Items requested by one typed stream read; the host also bounds the batch by bytes. */
  const ITEM_BATCH = 256;
  /** Shared with the SDK's `readChunks`; jco's declared per-byte iterator stays standard. */
  const READ_CHUNK = Symbol.for("tembra.wasi.stream.read-chunk");
  /** Shared with the SDK's `writeChunks`: a `stream<u8>` input that yields Uint8Array chunks. */
  const WRITE_CHUNKS = Symbol.for("tembra.wasi.stream.write-chunks");

  /** jco's error shape for a top-level `result` error. */
  class ComponentError extends Error {
    constructor(readonly payload: unknown) {
      super(`standard WASI call failed: ${JSON.stringify(payload)}`);
    }
  }

  /** Start a binary packet for one operation. */
  function packet(operation: number): Writer {
    const writer = new Writer();
    writer.u8(STANDARD_PACKET);
    writer.u8(operation);
    return writer;
  }

  /** Release a host handle exactly once. */
  function release(target: object): void {
    const owner = owners.get(target);
    if (!owner) throw new Error("handle was already dropped");
    if (owner.borrows) throw new Error("resource has outstanding borrows");
    const writer = packet(DROP);
    writer.handle(owner.handle);
    syncBytes(writer.finish());
    owners.delete(target);
    forget(owner);
  }

  /** Stop finalization after deterministic drop or ownership transfer. */
  function forget(owner: Owner): void {
    owner.released = true;
    collected.delete(owner);
    finalizers.unregister(owner);
  }

  /**
   * Best-effort GC cleanup supplements explicit `using`; it never borrows fresh authority. At
   * callback end (`ending`) every unfinished input pump returns its iterator and drops its
   * writer, so no pump outlives the callback that started it.
   */
  function sweep(ending = false): void {
    if (ending) for (const pump of [...pumps]) pump.close();
    for (const owner of collected) {
      if (owner.released) {
        collected.delete(owner);
        continue;
      }
      if (owner.borrows) continue;
      const writer = packet(DROP);
      writer.handle(owner.handle);
      try {
        syncBytes(writer.finish());
        forget(owner);
      } catch {
        // No active scope, or an upstream borrow still holds it: retry at the next boundary.
      }
    }
  }

  const classes = RESOURCES.map((name) => {
    const resource = class {
      constructor() {
        throw new TypeError(`${name} is issued by the host`);
      }
      [dispose]() {
        release(this);
      }
    };
    Object.defineProperty(resource, "name", { value: name.split("#")[1] });
    return resource;
  });

  /** Read raw byte chunks through the generic stream operation. */
  class ByteStream {
    #ended = false;
    /** Return the next chunk, or undefined at end of stream. */
    async [READ_CHUNK](): Promise<Uint8Array | undefined> {
      if (this.#ended) return undefined;
      const owner = owners.get(this);
      if (!owner) throw new Error("stream was dropped");
      const writer = packet(STREAM_READ);
      writer.handle(owner.handle);
      writer.u32(STREAM_CHUNK);
      const reader = new Reader(await __hostAsync(writer.finish()));
      const chunk = reader.u8() === 0 ? reader.bytes() : undefined;
      reader.end();
      if (chunk === undefined) this.#close();
      return chunk;
    }
    /** Iterate bytes as declared by jco for `stream<u8>`; transfer remains chunked. */
    async *[Symbol.asyncIterator](): AsyncGenerator<number> {
      try {
        for (let chunk = await this[READ_CHUNK](); chunk; chunk = await this[READ_CHUNK]())
          yield* chunk;
      } finally {
        this.#close();
      }
    }
    #close(): void {
      if (owners.has(this)) release(this);
      this.#ended = true;
    }
    [dispose]() {
      this.#close();
    }
  }

  /** A typed `stream<T>` read in bounded batches of whole encoded items. */
  class ItemStream {
    #ended = false;
    constructor(readonly item: number) {}
    /** Return the next batch, or undefined at end of stream. */
    async next(): Promise<unknown[] | undefined> {
      if (this.#ended) return undefined;
      const owner = owners.get(this);
      if (!owner) throw new Error("stream was dropped");
      const writer = packet(STREAM_READ);
      writer.handle(owner.handle);
      writer.u32(ITEM_BATCH);
      const reader = new Reader(await __hostAsync(writer.finish()));
      if (reader.u8() !== 0) {
        reader.end();
        this.#close();
        return undefined;
      }
      const items: unknown[] = [];
      for (let count = reader.u32(); count > 0; count--)
        items.push(decode(reader, this.item, handles));
      reader.end();
      return items;
    }
    async *[Symbol.asyncIterator](): AsyncGenerator<unknown> {
      try {
        for (let items = await this.next(); items; items = await this.next()) yield* items;
      } finally {
        this.#close();
      }
    }
    #close(): void {
      if (owners.has(this)) release(this);
      this.#ended = true;
    }
    [dispose]() {
      this.#close();
    }
  }

  /** A failed STREAM_WRITE: the consumer is gone and its future reports why. */
  class WriteFailure extends Error {}

  /** Callback-owned pumps of `stream<u8>` inputs; each ends with its writer's DROP. */
  const pumps = new Set<Pump>();

  /** Pump one guest iterable into a host input stream, one bounded chunk at a time. */
  class Pump {
    #closed = false;
    #iterator: Iterator<unknown> | AsyncIterator<unknown> | undefined;
    #chunked = true;

    constructor(
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
      pumps.add(this);
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
        await __hostAsync(writer.finish());
      } catch (error) {
        throw new WriteFailure(String(error));
      }
    }

    /** Return an unfinished iterator and drop the writer: the input reaches end of stream. */
    close(): void {
      if (this.#closed) return;
      this.#closed = true;
      pumps.delete(this);
      const iterator = this.#iterator;
      this.#iterator = undefined;
      try {
        const returned: any = iterator?.return?.();
        if (typeof returned?.then === "function") returned.then(undefined, () => {});
      } catch {
        // The source's cleanup failure cannot reopen the stream.
      }
      try {
        drop(this.handle);
      } catch {
        // Outside an active scope the host closes the writer at callback end.
      }
    }
  }

  /** DROP a host identity that the guest never published as an object. */
  function drop(handle: number): void {
    const writer = packet(DROP);
    writer.handle(handle);
    syncBytes(writer.finish());
  }

  /** A one-shot standard future exposed as jco's PromiseLike. */
  class StandardFuture {
    #value: Promise<unknown> | undefined;
    /** Input pumps of the call that issued this future; a source failure rejects it. */
    #pumps: Promise<unknown>[] = [];
    constructor(readonly payload: number) {}
    link(pumps: Promise<unknown>[]): void {
      this.#pumps = pumps;
    }
    then(resolve?: (value: any) => unknown, reject?: (error: any) => unknown) {
      if (!this.#value) {
        const owner = owners.get(this);
        if (!owner) return Promise.reject(new Error("future was dropped")).then(resolve, reject);
        owners.delete(this);
        forget(owner);
        const writer = packet(FUTURE_READ);
        writer.handle(owner.handle);
        this.#value = __hostAsync(writer.finish()).then(async (bytes) => {
          const reader = new Reader(bytes);
          const value = this.payload < 0 ? undefined : decode(reader, this.payload, handles);
          reader.end();
          for (const failure of await Promise.all(this.#pumps))
            if (failure !== undefined) throw failure;
          return value;
        });
      }
      return this.#value.then(resolve, reject);
    }
    // jco declares futures as `Promise`; reading starts on first use of any of these.
    catch(reject?: (error: any) => unknown) {
      return this.then(undefined, reject);
    }
    finally(callback?: () => void) {
      return Promise.resolve(this).finally(callback);
    }
    /** Drop an unread future; a consumed one has no host identity left. */
    [dispose]() {
      if (owners.has(this)) release(this);
    }
  }

  const handles: HandleCodec = {
    handle(kind, detail, value) {
      const owner = typeof value === "object" && value ? owners.get(value) : undefined;
      if (!owner || ((kind === "own" || kind === "borrow") && owner.resource !== detail))
        throw new TypeError(`expected a live ${RESOURCES[detail as number] ?? kind} handle`);
      if (kind === "own") {
        if (owner.borrows) throw new Error("resource has outstanding borrows");
        owners.delete(value as object);
        forget(owner);
      }
      return owner.handle;
    },
    value(kind, detail, handle) {
      const target =
        kind === "stream"
          ? detail === "u8"
            ? new ByteStream()
            : new ItemStream(detail as number)
          : kind === "future"
            ? new StandardFuture(detail as number)
            : Object.create(classes[detail as number]!.prototype);
      const owner = {
        handle,
        resource: kind === "own" ? (detail as number) : -1,
        borrows: 0,
        released: false,
      };
      owners.set(target, owner);
      finalizers.register(target, owner, owner);
      return target;
    },
  };

  /** Decode a reply and unwrap a top-level result as jco does. */
  function reply(index: number, bytes: Uint8Array): unknown {
    const method = METHODS[index]!;
    if (method.result === undefined) return undefined;
    const reader = new Reader(bytes);
    const value = decode(reader, method.result, handles);
    reader.end();
    if (TYPES[method.result]![0] === "result") {
      if (value.tag === "err") throw new ComponentError(value.val);
      return value.val;
    }
    return value;
  }

  /** Create a host input stream for one `stream<u8>` argument. */
  function input(detail: unknown, source: any): Pump {
    if (
      !(source instanceof Uint8Array) &&
      typeof source?.[WRITE_CHUNKS] !== "function" &&
      typeof source?.[READ_CHUNK] !== "function" &&
      typeof source?.[Symbol.asyncIterator] !== "function" &&
      typeof source?.[Symbol.iterator] !== "function"
    )
      throw new TypeError("stream<u8> arguments require an AsyncIterable<number>");
    const type = TYPES.findIndex(([kind, item]) => kind === "stream" && item === detail);
    const writer = packet(STREAM_NEW);
    writer.u32(type);
    const reader = new Reader(syncBytes(writer.finish()));
    const handle = reader.handle();
    reader.end();
    return new Pump(handle, source);
  }

  /** Encode arguments and preserve the WIT calling convention. */
  function invoke(index: number, args: unknown[]): unknown {
    sweep();
    const method = METHODS[index]!;
    if (args.length !== method.params.length)
      throw new TypeError(`${method.name} expects ${method.params.length} arguments`);
    const writer = packet(CALL);
    writer.u32(index);
    const borrowed: Owner[] = [];
    const inputs: Pump[] = [];
    const codec: HandleCodec = {
      ...handles,
      handle(kind, detail, value) {
        if (kind === "stream") {
          const pump = input(detail, value);
          inputs.push(pump);
          return pump.handle;
        }
        const id = handles.handle(kind, detail, value);
        if (kind === "borrow" && method.asynchronous) {
          const owner = owners.get(value as object)!;
          owner.borrows++;
          borrowed.push(owner);
        }
        return id;
      },
    };
    const release = () => {
      for (const owner of borrowed) owner.borrows--;
    };
    /** Start pumps once the host accepted the packet; a source failure rejects the future. */
    const started = (value: unknown) => {
      const running = inputs.map((pump) => pump.run());
      if (value instanceof StandardFuture) value.link(running);
      return value;
    };
    let sent = false;
    try {
      method.params.forEach((type, position) => encode(writer, type, args[position], codec));
      if (method.asynchronous) {
        const result = __hostAsync(writer.finish());
        sent = true;
        const running = inputs.map((pump) => pump.run());
        return result
          .then((bytes) => {
            const value = reply(index, bytes);
            if (value instanceof StandardFuture) value.link(running);
            return value;
          })
          .finally(release);
      }
      const bytes = syncBytes(writer.finish());
      sent = true;
      return started(reply(index, bytes));
    } catch (error) {
      release();
      // A refused call transferred nothing: discard its untransferred writers.
      if (!sent) for (const pump of inputs) pump.close();
      throw error;
    }
  }

  const modules: Record<string, Record<string, unknown>> = {};
  for (const [index, method] of METHODS.entries()) {
    if (!imported.has(method.interface)) continue;
    const module = (modules[method.interface] ??= {});
    if (method.resource === undefined) {
      module[method.name] = (...args: unknown[]) => invoke(index, args);
      continue;
    }
    const resource = classes[method.resource]!;
    module[resource.name] = resource;
    Object.defineProperty(resource.prototype, method.name, {
      value(this: object, ...args: unknown[]) {
        return invoke(index, [this, ...args]);
      },
    });
  }
  // Resource classes declared by an imported interface stay reachable through its module.
  for (const [index, name] of RESOURCES.entries()) {
    const [contract] = name.split("#");
    const filesystem = contract === "wasi:filesystem/types@0.3.0";
    if (imported.has(contract!) || (filesystem && imported.has("wasi:filesystem/preopens@0.3.0")))
      (modules[contract!] ??= {})[classes[index]!.name] = classes[index];
  }
  globalThis.__tembraStandard = Object.freeze(
    Object.fromEntries(
      Object.entries(modules).map(([name, value]) => [name, Object.freeze(value)]),
    ),
  );
  globalThis.__tembraSweepStandard = sweep;
})();
