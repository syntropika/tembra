// Standard WASI bindings interpreted from the table generated from WASI WIT at build time.
// The guest owns only handle numbers; the host validates every handle before using it.
import { METHODS, RESOURCES, TYPES } from "../../../../packages/ts/sdk/src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  FUTURE_READ,
  FUTURE_WRITE,
  type HandleCodec,
  Reader,
  STANDARD_PACKET,
  STREAM_CHUNK,
  STREAM_READ,
  Writer,
  decode,
  encode,
  resourceClassName,
} from "../../../../packages/ts/sdk/src/interpreter/standard-codec.ts";

import { InputBindings } from "./standard-inputs.ts";

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

  /** jco's error shape for a top-level `result` error. */
  class ComponentError extends Error {
    constructor(readonly payload: unknown) {
      super(
        `standard WASI call failed: ${JSON.stringify(payload, (_key, value) => (typeof value === "bigint" ? value.toString() : value))}`,
      );
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
    if (ending) inputs.close();
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

  const classes = RESOURCES.map((name, index) => {
    const resource = class {
      constructor(...args: unknown[]) {
        const constructor = METHODS.findIndex(
          (method) => method.resource === index && method.kind === "constructor",
        );
        if (constructor < 0) throw new TypeError(`${name} is issued by the host`);
        return invoke(constructor, args) as any;
      }
      [dispose]() {
        release(this);
      }
    };
    Object.defineProperty(resource, "name", { value: resourceClassName(name) });
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
        const writer = packet(FUTURE_READ);
        writer.handle(owner.handle);
        let pending: Promise<Uint8Array>;
        try {
          pending = __hostAsync(writer.finish());
        } catch (error) {
          return Promise.reject(error).then(resolve, reject);
        }
        owners.delete(this);
        forget(owner);
        this.#value = pending.then(
          async (bytes) => {
            const reader = new Reader(bytes);
            const value = this.payload < 0 ? undefined : decode(reader, this.payload, handles);
            reader.end();
            for (const failure of await Promise.all(this.#pumps))
              if (failure !== undefined) throw failure;
            return value;
          },
          (error) => {
            try {
              inputs.drop(owner.handle);
            } catch {
              // A host refusal may leave an unread identity; consumed futures have none.
            }
            throw error;
          },
        );
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
  function reply(index: number, bytes: Uint8Array, running: Promise<unknown>[]): unknown {
    const method = METHODS[index]!;
    if (method.result === undefined) return undefined;
    const reader = new Reader(bytes);
    const value = decode(reader, method.result, handles);
    reader.end();
    link(method.result, value, running);
    if (TYPES[method.result]![0] === "result") {
      if (value.tag === "err") throw new ComponentError(value.val);
      return value.val;
    }
    return value;
  }

  /** Attach producer failures to futures, including futures nested in a result tuple. */
  function link(type: number, value: any, running: Promise<unknown>[]): void {
    if (type < 0 || value === undefined || value === null) return;
    const [kind, a, b] = TYPES[type]!;
    if (kind === "future") (value as StandardFuture).link(running);
    else if (kind === "tuple")
      (a as number[]).forEach((item, at) => link(item, value[at], running));
    else if (kind === "record")
      for (const [name, item] of a as [string, number][]) link(item, value[name], running);
    else if (kind === "list") for (const item of value) link(a as number, item, running);
    else if (kind === "option") link(a as number, value, running);
    else if (kind === "result") link((value.tag === "ok" ? a : b) as number, value.val, running);
    else if (kind === "variant") {
      const item = (a as [string, number][]).find(([tag]) => tag === value.tag)?.[1];
      if (item !== undefined) link(item, value.val, running);
    }
  }

  /** Visit handle-bearing values without reaching into resource implementations. */
  function discard(type: number, value: any): void {
    if (type < 0 || value === undefined || value === null) return;
    const [kind, a, b] = TYPES[type]!;
    if (kind === "own") {
      if (owners.has(value))
        try {
          release(value);
        } catch {
          // If the callback already ended, the host still owns the revision's table entry.
        }
    } else if (kind === "tuple") (a as number[]).forEach((item, at) => discard(item, value[at]));
    else if (kind === "record")
      for (const [name, item] of a as [string, number][]) discard(item, value[name]);
    else if (kind === "list") for (const item of value) discard(a as number, item);
    else if (kind === "option") discard(a as number, value);
    else if (kind === "result") discard((value.tag === "ok" ? a : b) as number, value.val);
    else if (kind === "variant") {
      const item = (a as [string, number][]).find(([tag]) => tag === value.tag)?.[1];
      if (item !== undefined) discard(item, value.val);
    }
  }

  /** Validate an entire value before committing any owned-resource transfer. */
  function claims() {
    const owned = new Map<object, Owner>();
    const borrowed = new Set<Owner>();
    const codec: HandleCodec = {
      ...handles,
      handle(kind, detail, value) {
        const id = handles.handle(kind, detail, value);
        const owner = owners.get(value as object)!;
        if (kind === "own") {
          if (owner.borrows || borrowed.has(owner))
            throw new Error("resource has outstanding borrows");
          if (owned.has(value as object)) throw new Error("resource is transferred twice");
          owned.set(value as object, owner);
        } else if (kind === "borrow") {
          if (owned.has(value as object)) throw new Error("resource is transferred and borrowed");
          borrowed.add(owner);
        }
        return id;
      },
    };
    return {
      codec,
      commit() {
        for (const [target, owner] of owned) {
          owners.delete(target);
          forget(owner);
        }
      },
      abandon() {
        // A queued packet can still be refused by host admission before it takes ownership.
        // Accepted resources have no table identity left; only untaken inputs can be dropped.
        for (const owner of owned.values()) {
          const writer = packet(DROP);
          writer.handle(owner.handle);
          try {
            syncBytes(writer.finish());
          } catch {
            // Already taken, or the scope ended: the host owns its physical cleanup.
          }
        }
      },
      lend() {
        for (const owner of borrowed) owner.borrows++;
      },
      release() {
        for (const owner of borrowed) owner.borrows--;
      },
    };
  }

  const inputs = new InputBindings(
    syncBytes,
    (bytes) => __hostAsync(bytes),
    (handle, payload, value) => {
      const transaction = claims();
      const writer = packet(FUTURE_WRITE);
      writer.handle(handle);
      encode(writer, payload, value, transaction.codec);
      syncBytes(writer.finish());
      transaction.commit();
    },
    discard,
  );

  /** Encode all arguments, then transfer ownership when the packet is accepted. */
  function invoke(index: number, args: unknown[]): unknown {
    sweep();
    const method = METHODS[index]!;
    if (args.length !== method.params.length)
      throw new TypeError(`${method.name} expects ${method.params.length} arguments`);
    const writer = packet(CALL);
    writer.u32(index);
    const transaction = claims();
    const writers = inputs.call();
    const codec: HandleCodec = {
      ...transaction.codec,
      handle: (kind, detail, value) =>
        kind === "stream" || kind === "future"
          ? writers.handle(kind, detail, value)
          : transaction.codec.handle(kind, detail, value),
    };
    let sent = false;
    let lent = false;
    try {
      method.params.forEach((type, at) => encode(writer, type, args[at], codec));
      if (method.asynchronous) {
        transaction.lend();
        lent = true;
        const result = __hostAsync(writer.finish());
        sent = true;
        transaction.commit();
        const running = writers.start();
        return result
          .then(
            (bytes) => reply(index, bytes, running),
            (error) => {
              transaction.abandon();
              writers.close();
              throw error;
            },
          )
          .finally(transaction.release);
      }
      const bytes = syncBytes(writer.finish());
      sent = true;
      transaction.commit();
      return reply(index, bytes, writers.start());
    } catch (error) {
      if (lent) transaction.release();
      if (!sent) writers.close();
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
    if (method.kind === "constructor") continue;
    Object.defineProperty(method.kind === "static" ? resource : resource.prototype, method.name, {
      value(this: object, ...args: unknown[]) {
        return invoke(index, method.kind === "static" ? args : [this, ...args]);
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
