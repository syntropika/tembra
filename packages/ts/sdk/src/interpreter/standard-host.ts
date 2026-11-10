/**
 * Browser host of the generated standard WASI calls (private binary wire).
 *
 * Packets come from the same generated table and codec that the guest bindings and the native
 * dispatcher use. One instance belongs to one Orb revision: it admits only the interfaces that
 * revision imported, issues opaque handle numbers and validates every incoming handle for
 * identity, liveness and type before any implementation sees it. This module owns transport
 * and ownership only; method semantics live in `standard-filesystem.ts`, `standard-system.ts`,
 * `standard-cli.ts` and `standard-http.ts`, keyed by the generated method names.
 *
 * Ownership rules: owned resources, streams and futures belong to the revision. Every handle a
 * packet names (owned, borrowed, stream or future) is claimed and checked for aliasing while
 * the packet decodes; nothing is transferred until the whole packet validated, so a refused
 * packet consumes nothing. An owned parameter moves the table's reference to the call: the
 * implementation retains what it keeps, and the host releases the transfer reference once the
 * implementation was invoked. Asynchronous calls lend their borrowed resources until they
 * settle. Input writers (streams and futures) created during a callback end with it: an
 * unfinished stream reaches end of stream and an unwritten future resolves to its abandoned
 * error, so no consumer outlives its producer's callback or mistakes it for completion.
 * Callback cancellation settles pending calls but never kills a provider promise: physical
 * work stays tracked until the provider returns.
 */
import { METHODS, RESOURCES, TYPES, WRITABLE_FUTURES } from "./standard.ts";
import {
  CALL,
  DROP,
  FUTURE_NEW,
  FUTURE_READ,
  FUTURE_WRITE,
  type HandleCodec,
  INGRESS_HEAD,
  Reader,
  STANDARD_PACKET,
  STREAM_CHUNK,
  STREAM_NEW,
  STREAM_READ,
  STREAM_WRITE,
  Writer,
  decode,
  encode,
} from "./standard-codec.ts";
import { filesystemMethods, type StandardMount } from "./standard-filesystem.ts";
import { StandardCli, type StdioStreams } from "./standard-cli.ts";
import { httpMethods, type HttpClient } from "./standard-http.ts";
import type { IngressExchange } from "./http-ingress.ts";
import { IngressCall } from "./standard-ingress.ts";
import type { HttpResponse } from "./standard-http-values.ts";
import { socketMethods, type SocketBindings } from "./standard-sockets.ts";
import type { CliOptions } from "../authoring/types.ts";
import {
  abortable,
  cancelled,
  HostFuture,
  type Implementation,
  InputBudget,
  InputStream,
  methodKey,
  OutputStream,
  type StandardResource,
  type StandardServices,
  WritableFuture,
} from "./standard-streams.ts";
import {
  browserClocks,
  browserRandom,
  checkedClocks,
  revisionRandom,
  systemMethods,
  type ClockProvider,
  type RandomProvider,
  type SystemProviders,
} from "./standard-system.ts";

export type { StandardMount } from "./standard-filesystem.ts";

/** Live handles per revision, matching the native table. */
const HANDLE_LIMIT = 4096;
const NONE = -1;
/** The only qualified input stream type. */
const BYTE_STREAM = TYPES.findIndex(([kind, item]) => kind === "stream" && item === "u8");
/** `wasi:http/handler#handle`'s result, the same generated type `client.send` returns. */
const HANDLER_RESULT = METHODS.find(
  (method) => method.interface === "wasi:http/client@0.3.0" && method.name === "send",
)!.result!;
const HTTP_REQUEST = RESOURCES.indexOf("wasi:http/types@0.3.0#request");
/** What an unwritten writable future resolves to; every writable payload has this error case. */
const ABANDONED = { tag: "err", val: { tag: "internal-error", val: "callback ended" } } as const;

/**
 * Handle numbers are issued once per realm and never reused, so a number taken from another
 * revision, or a dropped one, can never name a live entry of this table.
 */
let issued = 0;

type Entry =
  | { kind: "own"; resource: number; value: StandardResource }
  | { kind: "output"; item: number | undefined; value: OutputStream }
  | { kind: "input"; value: InputStream }
  | { kind: "future"; payload: number; value: HostFuture }
  | { kind: "writable"; value: WritableFuture };

/** Fixed handle slots a value of `type` publishes; lists are bounded by rollback instead. */
function slots(type: number): number {
  const [kind, a, b] = TYPES[type]!;
  switch (kind) {
    case "own":
    case "stream":
    case "future":
      return 1;
    case "tuple":
      return (a as number[]).reduce((sum, item) => sum + slots(item), 0);
    case "record":
      return (a as [string, number][]).reduce((sum, [, item]) => sum + slots(item), 0);
    case "option":
      return slots(a as number);
    case "result":
      return Math.max(a === NONE ? 0 : slots(a as number), b === NONE ? 0 : slots(b as number));
    case "variant":
      return Math.max(
        0,
        ...(a as [string, number][]).map(([, item]) => (item === NONE ? 0 : slots(item))),
      );
    default:
      return 0;
  }
}

/** Codec for values that must not carry handles, such as typed stream items. */
const HANDLE_FREE: HandleCodec = {
  handle() {
    throw new Error("handle-bearing stream items are unsupported");
  },
  value() {
    throw new Error("handle-bearing stream items are unsupported");
  },
};

/** Handles named by one packet, checked while it decodes and committed only once it is valid. */
interface Claims {
  readonly codec: HandleCodec;
  /** Resources whose table reference moves to the receiver on commit. */
  readonly owned: StandardResource[];
  /** Resources borrowed by the packet. */
  readonly borrowed: StandardResource[];
  commit(): void;
}

/** Encoded result and the handles it publishes only when resumed into the guest. */
export interface StandardDelivery {
  readonly bytes: Uint8Array;
  readonly issued: readonly number[];
  /** Streams advanced by this reply; an unpublished read poisons its stream. */
  readonly reads?: readonly number[] | undefined;
}

/** Per-revision standard WASI host for the browser. */
export class StandardHost implements StandardServices {
  readonly clocks: ClockProvider;
  readonly random: RandomProvider;
  readonly #cli: StandardCli;
  readonly #entries = new Map<number, Entry>();
  /** Handle slots held by owned-item reads whose pull is still running. */
  #reserved = 0;
  readonly #work = new Set<Promise<unknown>>();
  readonly #implementations: (Implementation | undefined)[];
  readonly #allowed: boolean[];
  /** Input writers (streams and futures) created during the current callback. */
  readonly #inputs = new Set<number>();
  /** Accepted guest input across all of this revision's writers. */
  readonly #budget = new InputBudget();
  /** The incoming HTTP request the current callback serves. */
  #ingress: IngressCall | undefined;
  #closed = false;

  /**
   * @param allowed interfaces imported by this revision's checked contract
   * @param mounts filesystem grants opened for this revision, in grant order
   */
  constructor(
    allowed: ReadonlySet<string>,
    readonly mounts: readonly StandardMount[],
    readonly packetBytes: number,
    providers: SystemProviders & {
      cli?: CliOptions | undefined;
      stdio?: StdioStreams | undefined;
      /** Granted outgoing HTTP; absent means every `wasi:http/client#send` is denied. */
      http?: HttpClient | undefined;
      /** Providers and grants bound to this revision's `wasi:sockets` imports. */
      sockets?: SocketBindings | undefined;
    } = {},
  ) {
    this.clocks = checkedClocks(providers.clocks ?? browserClocks());
    this.random = revisionRandom(providers.random ?? browserRandom());
    this.#cli = new StandardCli(providers.cli, providers.stdio);
    const implementations: Record<string, Implementation> = {
      ...systemMethods(this.clocks, this.random),
      ...filesystemMethods(() => this.mounts),
      ...this.#cli.methods,
      ...httpMethods(providers.http),
      ...socketMethods(providers.sockets ?? {}),
    };
    this.#allowed = METHODS.map((method) => allowed.has(method.interface));
    this.#implementations = METHODS.map((method, index) =>
      this.#allowed[index]
        ? (implementations[methodKey(method)] ??
          implementations[`${method.interface}#${method.name}`])
        : undefined,
    );
  }

  /** Whether the revision is retired; late physical results are then released, not issued. */
  get closed(): boolean {
    return this.#closed;
  }

  /** Live handles; used by acceptance to observe retirement. */
  get size(): number {
    return this.#entries.size;
  }

  /** Accepted, unconsumed guest input bytes; bounded by `INPUT_BUDGET`. */
  get inputBytes(): number {
    return this.#budget.used;
  }

  /** Physical operations and destructors still running; zero once retirement settled. */
  get pending(): number {
    return this.#work.size;
  }

  /** Retain physical work even when the caller no longer accepts its result. */
  #track<T>(work: Promise<T>): Promise<T> {
    this.#work.add(work);
    const done = () => this.#work.delete(work);
    work.then(done, done);
    return work;
  }

  /** Wait for actual provider work and destructors before releasing a callback or revision. */
  async settled(): Promise<void> {
    while (this.#work.size) await Promise.allSettled([...this.#work]);
  }

  physical<T>(signal: AbortSignal | undefined, operation: () => Promise<T>): Promise<T> {
    if (this.#closed) return Promise.reject(new Error("Standard host is closed"));
    if (signal?.aborted) return Promise.reject(cancelled());
    return this.#track((async () => operation())());
  }

  cleanup(operation: () => unknown): void {
    this.#track((async () => operation())().catch(() => {}));
  }

  /** Issue a fresh handle for an outgoing resource, stream or future. */
  #issue(entry: Entry): number {
    const full = this.#entries.size + this.#reserved >= HANDLE_LIMIT;
    if (full || issued >= Number.MAX_SAFE_INTEGER)
      throw new Error(
        full ? "standard handle table is full" : "standard handle space is exhausted",
      );
    this.#entries.set(++issued, entry);
    return issued;
  }

  /** Release a value of a host future whenever it arrives; nobody will read it. */
  #orphan(future: HostFuture, payload: number): void {
    future.value.then(
      (value) => this.#releaseValue(payload, value),
      () => {},
    );
  }

  /**
   * Release whatever an entry holds. `undelivered` marks a handle whose publishing reply never
   * reached the guest (or a retiring revision): its future work is cancelled, not just orphaned.
   */
  #release(entry: Entry, undelivered = false): void {
    switch (entry.kind) {
      case "own":
        return entry.value.release();
      case "output":
      case "input":
        return entry.value.close();
      case "future":
        if (undelivered) entry.value.cancel?.();
        return this.#orphan(entry.value, entry.payload);
      case "writable":
        entry.value.abandon();
        if (!entry.value.transferred) entry.value.close();
        return;
    }
  }

  /** Release host values of `type` that were never issued as handles. */
  #releaseValue(type: number | undefined, value: any, issued?: ReadonlySet<unknown>): void {
    if (type === undefined || type === NONE || value === undefined || value === null) return;
    const [kind, a, b] = TYPES[type]!;
    switch (kind) {
      case "own":
      case "stream":
      case "future":
        if (issued?.has(value)) return;
        if (value instanceof OutputStream || value instanceof InputStream) value.close();
        else if (value instanceof HostFuture) {
          value.cancel?.();
          this.#orphan(value, a as number);
        } else (value as StandardResource).release();
        return;
      case "list":
        for (const item of value) this.#releaseValue(a as number, item, issued);
        return;
      case "tuple":
        (a as number[]).forEach((item, position) =>
          this.#releaseValue(item, value[position], issued),
        );
        return;
      case "record":
        for (const [name, item] of a as [string, number][])
          this.#releaseValue(item, value[name], issued);
        return;
      case "option":
        return this.#releaseValue(a as number, value, issued);
      case "result":
        return this.#releaseValue((value.tag === "ok" ? a : b) as number, value.val, issued);
      case "variant": {
        const found = (a as [string, number][]).find(([tag]) => tag === value.tag);
        return this.#releaseValue(found?.[1], value.val, issued);
      }
    }
  }

  readonly #handles: HandleCodec = {
    handle: (kind, detail, value) => {
      if (kind === "own" && typeof detail === "number" && RESOURCES[detail] !== undefined)
        return this.#issue({ kind: "own", resource: detail, value: value as StandardResource });
      if (kind === "stream" && value instanceof OutputStream)
        return this.#issue({
          kind: "output",
          item: detail === "u8" ? undefined : (detail as number),
          value,
        });
      if (kind === "future" && value instanceof HostFuture)
        return this.#issue({ kind: "future", payload: detail as number, value });
      throw new Error(`standard ${kind} results are unsupported in this browser host`);
    },
    value: () => {
      throw new Error("standard handles are decoded only as call parameters");
    },
  };

  /**
   * Claim every handle one packet names. An owned handle may appear once and never alongside a
   * borrow of itself, and it cannot move while a pending call has it lent; an input stream or
   * writable future readable end moves once. Nothing changes until `commit`.
   */
  #claims(): Claims {
    const commits: (() => void)[] = [];
    const owned: StandardResource[] = [];
    const borrowed: StandardResource[] = [];
    const moved = new Set<number>();
    const lent = new Set<number>();
    const codec: HandleCodec = {
      handle: this.#handles.handle,
      value: (kind, detail, handle) => {
        const entry = this.#entries.get(handle);
        if (!entry) throw new Error("unknown or dropped handle");
        if (moved.has(handle)) throw new Error("handle is passed more than once");
        switch (kind) {
          case "own":
          case "borrow":
            if (entry.kind !== "own" || entry.resource !== detail)
              throw new Error("handle has a different type");
            if (kind === "borrow") {
              lent.add(handle);
              borrowed.push(entry.value);
              return entry.value;
            }
            if (lent.has(handle)) throw new Error("handle is passed more than once");
            if (entry.value.borrows) throw new Error("handle is borrowed by a pending call");
            moved.add(handle);
            owned.push(entry.value);
            commits.push(() => this.#entries.delete(handle));
            return entry.value;
          case "stream":
            if (entry.kind !== "input" || TYPES[entry.value.type]![1] !== detail)
              throw new Error("handle has a different type");
            if (entry.value.transferred) throw new Error("stream was already transferred");
            moved.add(handle);
            commits.push(() => {
              entry.value.transferred = true;
            });
            return entry.value;
          case "future":
            if (entry.kind !== "writable" || TYPES[entry.value.type]![1] !== detail)
              throw new Error("handle has a different type");
            if (entry.value.transferred) throw new Error("future was already transferred");
            moved.add(handle);
            commits.push(() => {
              entry.value.transferred = true;
            });
            return entry.value;
          default:
            throw new Error(`standard ${kind} parameters are unsupported`);
        }
      },
    };
    return {
      codec,
      owned,
      borrowed,
      commit: () => {
        for (const commit of commits) commit();
      },
    };
  }

  /** Decode a call header and refuse methods this revision may not use. */
  #method(reader: Reader, asynchronous: boolean): number {
    const index = reader.u32();
    const method = METHODS[index];
    if (!method) throw new Error("unknown standard method");
    if (!this.#allowed[index]) throw new Error("standard interface is not imported by this Orb");
    if (method.asynchronous !== asynchronous)
      throw new Error("standard call uses the wrong calling convention");
    if (!this.#implementations[index])
      throw new Error(`${method.interface}#${method.name} is unavailable in this browser host`);
    return index;
  }

  /**
   * Decode and validate every argument, then commit transfers and loans together. A refused
   * packet (bad or aliased handle, trailing bytes, full table) consumes nothing. Returns the
   * owned parameters whose transfer reference the caller releases after invoking the method.
   */
  #arguments(
    index: number,
    reader: Reader,
    lent?: StandardResource[],
  ): { args: unknown[]; owned: StandardResource[] } {
    const claims = this.#claims();
    const method = METHODS[index]!;
    const args = method.params.map((type) => decode(reader, type, claims.codec));
    reader.end();
    const needed = method.result === undefined ? 0 : slots(method.result);
    if (this.#entries.size + this.#reserved + needed > HANDLE_LIMIT)
      throw new Error("standard handle table is full");
    claims.commit();
    for (const resource of lent ? claims.borrowed : []) {
      resource.borrows++;
      resource.retain();
      lent!.push(resource);
    }
    return { args, owned: claims.owned };
  }

  /** Encode a delivery; on failure roll back its new handles and release unissued values. */
  #deliver(
    type: number | undefined,
    value: unknown,
    head?: (writer: Writer) => void,
  ): StandardDelivery {
    const issued: number[] = [];
    const values = new Set<unknown>();
    const codec: HandleCodec = {
      ...this.#handles,
      handle: (kind, detail, item) => {
        const handle = this.#handles.handle(kind, detail, item);
        issued.push(handle);
        values.add(item);
        return handle;
      },
    };
    try {
      const writer = new Writer();
      head?.(writer);
      if (type !== undefined && type !== NONE) encode(writer, type, value, codec);
      const bytes = writer.finish();
      if (bytes.length > this.packetBytes)
        throw new Error("standard reply exceeds the packet bound");
      return { bytes, issued };
    } catch (error) {
      this.discard({ issued });
      this.#releaseValue(type, value, values);
      throw error;
    }
  }

  /** Serve one WIT-synchronous packet while the guest stack waits. */
  sync(packet: Uint8Array): Uint8Array {
    if (this.#closed) throw new Error("Standard host is closed");
    const reader = new Reader(packet);
    if (reader.u8() !== STANDARD_PACKET) throw new Error("not a standard packet");
    switch (reader.u8()) {
      case CALL: {
        const index = this.#method(reader, false);
        const { args, owned } = this.#arguments(index, reader);
        let value: unknown;
        try {
          value = this.#implementations[index]!({ host: this }, args);
        } finally {
          for (const resource of owned) resource.release();
        }
        return this.#deliver(METHODS[index]!.result, value).bytes;
      }
      case DROP: {
        const handle = reader.handle();
        reader.end();
        const entry = this.#entries.get(handle);
        if (!entry) throw new Error("unknown or dropped handle");
        if (entry.kind === "own" && entry.value.borrows)
          throw new Error("handle is borrowed by a pending call");
        this.#entries.delete(handle);
        this.#inputs.delete(handle);
        this.#release(entry);
        return new Uint8Array();
      }
      case STREAM_NEW: {
        const type = reader.u32();
        reader.end();
        // Only byte input streams are qualified; typed input streams are refused.
        if (type !== BYTE_STREAM) throw new Error("unsupported input stream type");
        const handle = this.#issue({ kind: "input", value: new InputStream(type, this.#budget) });
        this.#inputs.add(handle);
        const writer = new Writer();
        writer.handle(handle);
        return writer.finish();
      }
      case FUTURE_NEW: {
        const type = reader.u32();
        reader.end();
        if (!WRITABLE_FUTURES.includes(type)) throw new Error("unsupported writable future type");
        const payload = TYPES[type]![1] as number;
        const future = new WritableFuture<unknown>(type, ABANDONED, (value) =>
          this.#releaseValue(payload, value),
        );
        const handle = this.#issue({ kind: "writable", value: future });
        this.#inputs.add(handle);
        const writer = new Writer();
        writer.handle(handle);
        return writer.finish();
      }
      case FUTURE_WRITE: {
        const handle = reader.handle();
        const entry = this.#entries.get(handle);
        if (entry?.kind !== "writable") throw new Error("handle is not a live writable future");
        if (entry.value.written) throw new Error("future was already written");
        // The whole payload, including every owned handle inside it, validates first.
        const claims = this.#claims();
        const value = decode(reader, TYPES[entry.value.type]![1] as number, claims.codec);
        reader.end();
        if (claims.borrowed.length) throw new Error("future payloads cannot borrow");
        claims.commit();
        entry.value.write(value);
        return new Uint8Array();
      }
      default:
        throw new Error("unknown standard operation");
    }
  }

  /** Serve one asynchronous packet owned by the callback whose `signal` is supplied. */
  async async(packet: Uint8Array, signal?: AbortSignal): Promise<StandardDelivery> {
    if (this.#closed) throw new Error("Standard host is closed");
    const reader = new Reader(packet);
    if (reader.u8() !== STANDARD_PACKET) throw new Error("not a standard packet");
    switch (reader.u8()) {
      case CALL: {
        const index = this.#method(reader, true);
        // A cancelled callback starts nothing and transfers nothing.
        if (signal?.aborted) throw cancelled();
        const lent: StandardResource[] = [];
        const { args, owned } = this.#arguments(index, reader, lent);
        let release = () => {};
        // A loan is physical ownership: retirement waits until the call's provider work settles.
        if (lent.length)
          this.#track(
            new Promise<void>((resolve) => {
              release = resolve;
            }),
          );
        // Provider work started by this call; cancellation answers the guest promptly, but
        // the borrowed descriptors stay lent until that work returns.
        const work: Promise<unknown>[] = [];
        const host = this;
        const services: StandardServices = {
          packetBytes: this.packetBytes,
          get closed() {
            return host.closed;
          },
          physical: (signal, operation) => {
            const started = this.physical(signal, operation);
            work.push(started);
            return started;
          },
          cleanup: (operation) => this.cleanup(operation),
        };
        try {
          let started: unknown;
          try {
            started = this.#implementations[index]!({ host: services, signal }, args);
          } finally {
            // The implementation's first turn has run: it adopted what it keeps.
            for (const resource of owned) resource.release();
          }
          const value = await started;
          if (signal?.aborted || this.#closed) {
            this.#releaseValue(METHODS[index]!.result, value);
            throw cancelled();
          }
          return this.#deliver(METHODS[index]!.result, value);
        } finally {
          void Promise.allSettled(work).then(() => {
            for (const resource of lent) {
              resource.borrows--;
              resource.release();
            }
            release();
          });
        }
      }
      case STREAM_READ: {
        const handle = reader.handle();
        const requested = reader.u32();
        reader.end();
        const entry = this.#entries.get(handle);
        if (entry?.kind !== "output") throw new Error("handle is not a live output stream");
        const item = entry.item;
        if (item !== undefined && slots(item) > 0) {
          // Owned items: the item's handle slots are reserved before the pull, so no concurrent
          // read or issue can take them. The reservation is returned on every outcome, and on
          // success it becomes the handles this reply issues, with no await in between. Those
          // handles are journaled with the read, so an undelivered reply rolls them back and
          // poisons the stream, as every stream read does.
          let reserved = slots(item);
          if (this.#entries.size + this.#reserved + reserved > HANDLE_LIMIT)
            throw new Error("standard handle table is full");
          this.#reserved += reserved;
          const unreserve = () => {
            this.#reserved -= reserved;
            reserved = 0;
          };
          try {
            const taken = await this.#track(entry.value.take(signal));
            unreserve();
            if (!taken) return { bytes: Uint8Array.of(1), issued: [], reads: [handle] };
            // The value is this read's alone now; a drop or retirement since the pull means
            // nobody will receive it, so it is released here instead of being issued.
            if (this.#closed || entry.value.closed) {
              this.#releaseValue(item, taken.value);
              throw new Error(this.#closed ? "Standard host is closed" : "stream was dropped");
            }
            const delivery = this.#deliver(item, taken.value, (writer) => {
              writer.u8(0);
              writer.u32(1);
            });
            return { ...delivery, reads: [handle] };
          } finally {
            unreserve();
          }
        }
        const limit =
          item === undefined
            ? Math.min(Math.max(requested, 1), STREAM_CHUNK, this.packetBytes - 5)
            : Math.max(requested, 1);
        if (limit < 1) throw new Error("standard reply exceeds the packet bound");
        const encodeItem =
          item === undefined
            ? undefined
            : (value: unknown) => {
                const writer = new Writer();
                encode(writer, item, value, HANDLE_FREE);
                return writer.finish();
              };
        // Include queued reads, before their provider call starts, in retirement accounting.
        const bytes = await this.#track(
          entry.value.read(limit, this.packetBytes, signal, encodeItem),
        );
        return { bytes, issued: [], reads: [handle] };
      }
      case STREAM_WRITE: {
        const handle = reader.handle();
        const length = reader.u32();
        // Refuse a bad packet or stream before reserving budget or copying the chunk.
        if (length > STREAM_CHUNK) throw new Error("stream write exceeds the 64 KiB chunk bound");
        if (reader.remaining !== length)
          throw new RangeError(
            reader.remaining < length
              ? "standard packet is truncated"
              : "standard packet has trailing bytes",
          );
        const entry = this.#entries.get(handle);
        if (entry?.kind !== "input") throw new Error("handle is not a live input stream");
        await entry.value.write(length, () => reader.raw(length), signal);
        return { bytes: new Uint8Array(), issued: [] };
      }
      case INGRESS_HEAD: {
        const call = this.#ingress;
        if (!call) throw new Error("no incoming HTTP request is being served");
        if (call.exchange.published) throw new Error("the response head was already published");
        if (signal?.aborted) throw cancelled();
        // The whole result validates before the response moves to the host.
        const claims = this.#claims();
        const result = decode(reader, HANDLER_RESULT, claims.codec) as
          | { tag: "ok"; val: HttpResponse }
          | { tag: "err"; val: import("wasi:http/types@0.3.0").ErrorCode };
        reader.end();
        claims.commit();
        // The head leaves now; this operation completes only when the delivery ended, which
        // keeps the handler's callback (and its context authority) alive while it streams.
        const ended = call.respond(this, result);
        try {
          await abortable(ended, signal);
        } catch (error) {
          call.end(error);
          throw error;
        }
        return { bytes: new Uint8Array(), issued: [] };
      }
      case FUTURE_READ: {
        const handle = reader.handle();
        reader.end();
        const entry = this.#entries.get(handle);
        if (entry?.kind !== "future") throw new Error("handle is not a live future");
        // A future value is read once; the handle is consumed now. A value that arrives after
        // this read was cancelled is released, never issued.
        this.#entries.delete(handle);
        let value: unknown;
        try {
          value = await abortable(entry.value.value, signal);
        } catch (error) {
          if (signal?.aborted) this.#orphan(entry.value, entry.payload);
          throw error;
        }
        if (signal?.aborted || this.#closed) {
          this.#releaseValue(entry.payload, value);
          throw cancelled();
        }
        return this.#deliver(entry.payload, value);
      }
      default:
        throw new Error("unknown standard operation");
    }
  }

  /** Release resources from a reply that was never resumed into the guest. */
  discard(delivery: Pick<StandardDelivery, "issued" | "reads">): void {
    for (const handle of delivery.reads ?? []) {
      const entry = this.#entries.get(handle);
      if (entry?.kind === "output") entry.value.abandon();
    }
    for (const handle of delivery.issued) {
      const entry = this.#entries.get(handle);
      if (!entry) continue;
      this.#entries.delete(handle);
      this.#release(entry, true);
    }
  }

  /**
   * End of the current callback: every input writer it created and did not drop ends, so its
   * consumer reaches end of stream (streams) or the abandoned error (futures) instead of
   * waiting for a producer that is gone.
   */
  /**
   * Begin serving one incoming request in the next callback: issue its `own<request>` handle.
   * Invalid input is refused before anything is issued.
   */
  beginIngress(exchange: IngressExchange): number {
    if (this.#closed) throw new Error("Standard host is closed");
    if (this.#ingress) throw new Error("an incoming HTTP request is already being served");
    const call = new IngressCall(exchange, (operation) => this.cleanup(operation));
    const request = call.resource();
    try {
      const handle = this.#issue({ kind: "own", resource: HTTP_REQUEST, value: request });
      this.#ingress = call;
      return handle;
    } catch (error) {
      request.release();
      throw error;
    }
  }

  /** The serving callback ended: whatever its delivery had not finished ends with `reason`. */
  endIngress(reason: unknown): void {
    const call = this.#ingress;
    this.#ingress = undefined;
    call?.end(reason);
  }

  endCallback(): void {
    for (const handle of this.#inputs) {
      const entry = this.#entries.get(handle);
      if (entry?.kind !== "input" && entry?.kind !== "writable") continue;
      this.#entries.delete(handle);
      this.#release(entry);
    }
    this.#inputs.clear();
  }

  /** Retire the revision: drop every handle after physical provider work has settled. */
  async dispose(): Promise<void> {
    this.#closed = true;
    this.endIngress(new Error("Standard host is closed"));
    this.#cli.retire();
    // Producers first, so no consumer waits for a writer that can no longer run.
    for (const [handle, entry] of this.#entries)
      if (entry.kind === "input" || entry.kind === "writable") {
        this.#entries.delete(handle);
        this.#release(entry);
      }
    this.#inputs.clear();
    await this.settled();
    const entries = [...this.#entries.values()];
    this.#entries.clear();
    for (const entry of entries) this.#release(entry, true);
    await this.settled();
    this.#cli.close(this);
    await this.settled();
    // Every consumer had its turn; input nobody will read returns to the budget.
    this.#budget.discard();
  }
}

/** Whether an interpreter packet is a binary standard packet rather than UTF-8 JSON. */
export function isStandardPacket(packet: Uint8Array): boolean {
  return packet.byteLength > 0 && packet[0] === STANDARD_PACKET;
}
