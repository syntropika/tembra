/**
 * Standard `wasi:sockets` (TCP, UDP) and `wasi:sockets/ip-name-lookup` for the browser
 * standard host, over an injected `SocketProvider` (`socket-provider.ts`). Browsers have no
 * ambient sockets.
 *
 * Every call follows one order, so a refusal never reaches the provider:
 * 1. argument and state checks, as pinned upstream `wasmtime-wasi` 51.0.0-rc.1;
 * 2. the exact grant of the provider bound to that WIT import (`access-denied`);
 * 3. provider capability (`not-supported`; name lookup `permanent-resolver-failure`);
 * 4. the provider call.
 *
 * Ownership follows WASI's shared model: a socket handle and each stream derived from it hold
 * one reference to the provider backend. Dropping the handle leaves its streams working; a
 * dropped receive stream shuts down reads; the last reference closes the backend. Accepted
 * sockets are independent. Every closing promise is physical work tracked by the host.
 */
import type {
  ErrorCode,
  IpAddress,
  IpAddressFamily,
  IpSocketAddress,
} from "wasi:sockets/types@0.3.0";
import type { ErrorCode as LookupErrorCode } from "wasi:sockets/ip-name-lookup@0.3.0";
import type { SocketAccess } from "./browser-network.ts";
import {
  type Address,
  checkedIpAddress,
  fromWit,
  hasFamily,
  isRemote,
  isUnicast,
  key,
  parseHostName,
  toWit,
  unspecified,
} from "./socket-addresses.ts";
import {
  NameLookupError,
  type SocketOptions,
  SocketProviderError,
  type TcpBackend,
  type TcpConnection,
  type TcpListener,
  type TcpOptions,
  type UdpBackend,
  type UdpOptions,
} from "./socket-provider.ts";
import { METHODS, TYPES } from "./standard.ts";
import { encode, type HandleCodec, Writer } from "./standard-codec.ts";
import {
  abortable,
  bindResource,
  cancelled,
  HostFuture,
  type Implementation,
  type InputStream,
  OutputStream,
  type StandardCall,
  type StandardResource,
  type StandardServices,
} from "./standard-streams.ts";

const TYPES_INTERFACE = "wasi:sockets/types@0.3.0";
const LOOKUP_INTERFACE = "wasi:sockets/ip-name-lookup@0.3.0";
/** Upstream `MAX_DATAGRAM_SIZE`. */
const MAX_DATAGRAM = 0xffff;
/** Upstream listen backlog default and clamp. */
const DEFAULT_BACKLOG = 128;
const MAX_BACKLOG = 0x7fffffff;

type Result<T, E> = { tag: "ok"; val: T } | { tag: "err"; val: E };
const ok = <T>(val: T): { tag: "ok"; val: T } => ({ tag: "ok", val });
const fail = (tag: ErrorCode["tag"]): { tag: "err"; val: ErrorCode } => ({
  tag: "err",
  val: { tag } as ErrorCode,
});

/** The providers bound to this revision's socket and lookup imports. */
export interface SocketBindings {
  readonly types?: SocketAccess | undefined;
  readonly lookup?: SocketAccess | undefined;
}

// ---------------------------------------------------------------------------------------------
// errors

/** The generated error type of a function's `result<_, E>`, when the table binds it. */
function errorType(contract: string, name: string): number | undefined {
  const method = METHODS.find((m) => m.interface === contract && m.name === name);
  const result = method?.result === undefined ? undefined : TYPES[method.result];
  return result?.[0] === "result" ? (result[2] as number) : undefined;
}
const NO_HANDLES: HandleCodec = {
  handle() {
    throw new Error("an error code carries no handles");
  },
  value() {
    throw new Error("an error code carries no handles");
  },
};

/** A provider code only when it encodes as the generated type; otherwise `other(message)`. */
function code<C extends { tag: string }>(
  error: unknown,
  type: number | undefined,
  typed: unknown,
): C {
  if (typed !== undefined && type !== undefined) {
    try {
      encode(new Writer(), type, typed, NO_HANDLES);
      return typed as C;
    } catch {
      /* Not a standard code: reported as `other` below. */
    }
  }
  return {
    tag: "other",
    val: error instanceof Error ? error.message : String(error),
  } as unknown as C;
}

const socketCode = (error: unknown): ErrorCode =>
  code(
    error,
    errorType(TYPES_INTERFACE, "connect"),
    error instanceof SocketProviderError ? error.code : undefined,
  );
const lookupCode = (error: unknown): LookupErrorCode =>
  code(
    error,
    errorType(LOOKUP_INTERFACE, "resolveAddresses"),
    error instanceof NameLookupError ? error.code : undefined,
  );

/** Run a synchronous provider call, reporting its failure as a standard result. */
function attempt<T>(operation: () => T): Result<T, ErrorCode> {
  try {
    return ok(operation());
  } catch (error) {
    return { tag: "err", val: socketCode(error) };
  }
}

/** A provider-returned address as a standard result; malformed is the provider's failure. */
function localAddress(value: IpSocketAddress | undefined): Result<IpSocketAddress, ErrorCode> {
  if (value === undefined) return fail("invalid-state");
  const address = fromWit(value);
  return address
    ? ok(toWit(address))
    : { tag: "err", val: { tag: "other", val: "invalid provider address" } };
}

/** A provider-returned address, validated; a malformed one is the provider's failure. */
function provided(value: unknown): Address {
  const address = fromWit(value);
  if (!address) throw new Error("invalid provider address");
  return address;
}

// ---------------------------------------------------------------------------------------------
// shared physical ownership

/** One provider backend shared by a socket handle and its streams; the last user closes it. */
class Shared<B extends { close(): Promise<void> }> {
  #users = 1;

  constructor(
    readonly host: StandardServices,
    readonly backend: B,
  ) {}

  retain(): void {
    this.#users++;
  }

  release(): void {
    if (--this.#users === 0) this.host.cleanup(() => this.backend.close());
  }
}

/** Handle-table reference counting, as for every standard resource. */
abstract class SocketResource implements StandardResource {
  borrows = 0;
  #users = 1;

  retain(): void {
    this.#users++;
  }

  release(): void {
    if (--this.#users === 0) this.close();
  }

  protected abstract close(): void;
}

/** Independent option access: an absent getter or setter is `not-supported`, not invented. */
function getOption<O, K extends keyof O>(
  backend: SocketOptions<O>,
  name: K,
  valid: (value: unknown) => boolean,
): Result<O[K], ErrorCode> {
  if (!backend.getOption) return fail("not-supported");
  const value = attempt(() => backend.getOption!(name));
  if (value.tag === "ok" && !valid(value.val))
    return { tag: "err", val: { tag: "other", val: `invalid provider ${String(name)}` } };
  return value;
}

function setOption<O, K extends keyof O>(
  backend: SocketOptions<O>,
  name: K,
  value: O[K],
): Result<void, ErrorCode> {
  if (!backend.setOption) return fail("not-supported");
  return attempt(() => backend.setOption!(name, value));
}

const isU8 = (v: unknown) => typeof v === "number" && Number.isInteger(v) && v >= 1 && v <= 0xff;
const isU32 = (v: unknown) =>
  typeof v === "number" && Number.isInteger(v) && v >= 1 && v <= 0xffffffff;
const isU64 = (v: unknown) => typeof v === "bigint" && v > 0n && v < 1n << 64n;
const isBool = (v: unknown) => typeof v === "boolean";

// ---------------------------------------------------------------------------------------------
// TCP

type TcpState =
  | { kind: "default" }
  | { kind: "connecting" }
  | { kind: "connected"; connection: TcpConnection }
  | { kind: "listening"; listener: TcpListener }
  | { kind: "closed"; code: ErrorCode };

/** Close a connection nobody will receive; each promise is its physical acknowledgement. */
function discardConnection(host: StandardServices, connection: TcpConnection): void {
  host.cleanup(() => connection.readable.cancel());
  host.cleanup(() => connection.writable.abort());
}

/** Validate a provider connection; the host never trusts its shape. */
function checkedConnection(value: unknown): TcpConnection {
  const connection = value as TcpConnection;
  if (
    !(connection?.readable instanceof ReadableStream) ||
    !(connection.writable instanceof WritableStream)
  )
    throw new Error("invalid provider connection");
  provided(connection.remote);
  return connection;
}

/** `wasi:sockets/types#tcp-socket`. */
export class TcpSocket extends SocketResource {
  #state: TcpState;
  #bound: boolean;
  #backlog = DEFAULT_BACKLOG;
  #sent = false;
  #received = false;

  constructor(
    readonly access: SocketAccess,
    readonly family: IpAddressFamily,
    readonly shared: Shared<TcpBackend>,
    connection?: TcpConnection,
  ) {
    super();
    this.#state = connection ? { kind: "connected", connection } : { kind: "default" };
    this.#bound = connection !== undefined;
  }

  protected close(): void {
    this.shared.release();
  }

  get #backend(): TcpBackend {
    return this.shared.backend;
  }

  get #grants() {
    return this.access.grants;
  }

  bind(local: IpSocketAddress): Result<void, ErrorCode> {
    if (this.#bound || this.#state.kind !== "default") return fail("invalid-state");
    const address = fromWit(local)!;
    if (!isUnicast(address) || !hasFamily(address, this.family)) return fail("invalid-argument");
    if (!this.#grants.allows(address, "tcp-bind")) return fail("access-denied");
    const backend = this.#backend;
    if (!backend.bind) return fail("not-supported");
    const bound = attempt(() => backend.bind!(toWit(address)));
    if (bound.tag === "ok") this.#bound = true;
    return bound;
  }

  async connect(remote: IpSocketAddress, call: StandardCall): Promise<Result<void, ErrorCode>> {
    if (this.#state.kind !== "default") return fail("invalid-state");
    const address = fromWit(remote)!;
    if (!isUnicast(address) || !isRemote(address) || !hasFamily(address, this.family))
      return fail("invalid-argument");
    if (!this.#bound && !this.#grants.allows(unspecified(this.family), "tcp-bind"))
      return fail("access-denied");
    if (!this.#grants.allows(address, "tcp-connect")) return fail("access-denied");
    this.#state = { kind: "connecting" };
    const abort = new AbortController();
    const stop = () => abort.abort();
    call.signal?.addEventListener("abort", stop, { once: true });
    const pending = call.host.physical(call.signal, () =>
      this.#backend.connect(toWit(address), abort.signal),
    );
    try {
      const connection = checkedConnection(await abortable(pending, call.signal));
      this.#state = { kind: "connected", connection };
      this.#bound = true;
      return ok(undefined);
    } catch (error) {
      this.#state = { kind: "closed", code: { tag: "invalid-state" } };
      if (!call.signal?.aborted) {
        const failure = socketCode(error);
        this.#state = { kind: "closed", code: failure };
        return { tag: "err", val: failure };
      }
      // A connection that arrives after cancellation is closed and never published.
      pending.then(
        (late) => discardConnection(call.host, late),
        () => {},
      );
      throw cancelled();
    } finally {
      call.signal?.removeEventListener("abort", stop);
    }
  }

  listen(call: StandardCall): Result<OutputStream, ErrorCode> {
    if (this.#state.kind !== "default") return fail("invalid-state");
    const implicit = unspecified(this.family);
    if (this.#bound) {
      const local = localAddress(this.#backend.local);
      if (local.tag === "err") return local;
      if (!this.#grants.allows(fromWit(local.val)!, "tcp-listen")) return fail("access-denied");
    } else if (
      !this.#grants.allows(implicit, "tcp-bind") ||
      !this.#grants.allows(implicit, "tcp-listen")
    )
      return fail("access-denied");
    const backend = this.#backend;
    if (!backend.listen) return fail("not-supported");
    const listened = attempt(() => backend.listen!(this.#backlog));
    if (listened.tag === "err") return listened;
    const listener = listened.val;
    if (typeof listener?.accept !== "function")
      return { tag: "err", val: { tag: "other", val: "invalid provider listener" } };
    this.#state = { kind: "listening", listener };
    this.#bound = true;
    return ok(this.#acceptStream(call.host, listener));
  }

  /**
   * The listen stream holds its own backend reference and one revision-owned accept pull at a
   * time. Dropping it aborts that pull, waits for its settlement (the physical acknowledgement),
   * then releases the reference; the socket handle's reference is independent.
   */
  #acceptStream(host: StandardServices, listener: TcpListener): OutputStream {
    this.shared.retain();
    const stop = new AbortController();
    let accepting: Promise<unknown> | undefined;
    const next = async (): Promise<TcpSocket> => {
      for (;;) {
        const pull = listener.accept(stop.signal);
        accepting = pull;
        const { socket, connection } = await pull;
        let checked: TcpConnection;
        try {
          checked = checkedConnection(connection);
          if (typeof socket?.close !== "function") throw new Error("invalid provider socket");
        } catch (error) {
          host.cleanup(() => socket?.close?.());
          throw error;
        }
        const child = new TcpSocket(this.access, this.family, new Shared(host, socket), checked);
        // Native accepts any peer once listening on a granted address (`tcp-accept`).
        if (this.#grants.allows(provided(checked.remote), "tcp-accept")) return child;
        child.release();
      }
    };
    return new OutputStream(
      {
        next,
        close: () => {
          stop.abort();
          const pending = accepting;
          host.cleanup(async () => {
            await pending?.then(
              () => {},
              () => {},
            );
            this.shared.release();
          });
        },
      },
      () => {},
      (value) => (value as StandardResource).release(),
    );
  }

  send(data: InputStream, call: StandardCall): HostFuture {
    if (this.#state.kind !== "connected" || this.#sent) {
      data.cancel();
      return HostFuture.of(fail("invalid-state"));
    }
    const connection = this.#state.connection;
    const locked = attempt(() => connection.writable.getWriter());
    if (locked.tag === "err") {
      data.cancel();
      return HostFuture.of(locked);
    }
    this.#sent = true;
    const host = call.host;
    const writer = locked.val;
    this.shared.retain();
    const value = (async (): Promise<Result<void, ErrorCode>> => {
      try {
        for (let chunk = await data.read(); chunk; chunk = await data.read())
          await writer.write(chunk);
        if (data.cancelled) throw new Error("send stopped");
        // A clean end of the guest stream is a write shutdown, acknowledged by its promise.
        const closing = writer.close();
        host.cleanup(() => closing);
        await closing;
        return ok(undefined);
      } catch (error) {
        data.cancel();
        const aborting = writer.abort(error);
        host.cleanup(() => aborting);
        return { tag: "err", val: socketCode(error) };
      } finally {
        this.shared.release();
      }
    })();
    return new HostFuture(value);
  }

  receive(call: StandardCall): [OutputStream, HostFuture] {
    const empty = { next: async () => undefined, close() {} };
    if (this.#state.kind !== "connected" || this.#received)
      return [new OutputStream(empty, () => {}), HostFuture.of(fail("invalid-state"))];
    const connection = this.#state.connection;
    const locked = attempt(() => connection.readable.getReader());
    if (locked.tag === "err") return [new OutputStream(empty, () => {}), HostFuture.of(locked)];
    this.#received = true;
    const host = call.host;
    const reader = locked.val;
    this.shared.retain();
    let settle!: (result: Result<void, ErrorCode>) => void;
    const done = new Promise<Result<void, ErrorCode>>((resolve) => {
      settle = resolve;
    });
    const stream = new OutputStream(
      {
        async next() {
          for (;;) {
            const { done: end, value } = await reader.read();
            if (end) return undefined;
            if (!(value instanceof Uint8Array)) throw new Error("invalid provider chunk");
            // Empty chunks are legal and are not the end of the stream.
            if (value.byteLength) return value.slice();
          }
        },
        close: (end) => {
          host.cleanup(async () => {
            // A drop or failure shuts down reads; its promise is the acknowledgement.
            if (end !== "eof") await reader.cancel();
            this.shared.release();
          });
        },
      },
      // Upstream: end of stream and an early drop are both `ok`; a failure is its code.
      (failure, end) =>
        settle(end === "failed" ? { tag: "err", val: socketCode(failure) } : ok(undefined)),
    );
    return [stream, new HostFuture(done)];
  }

  getLocalAddress(): Result<IpSocketAddress, ErrorCode> {
    if (this.#state.kind === "closed") return { tag: "err", val: this.#state.code };
    if (!this.#bound || this.#state.kind === "connecting") return fail("invalid-state");
    return localAddress(this.#backend.local);
  }

  getRemoteAddress(): Result<IpSocketAddress, ErrorCode> {
    if (this.#state.kind === "closed") return { tag: "err", val: this.#state.code };
    if (this.#state.kind !== "connected") return fail("invalid-state");
    return ok(toWit(provided(this.#state.connection.remote)));
  }

  getIsListening(): boolean {
    return this.#state.kind === "listening";
  }

  getAddressFamily(): IpAddressFamily {
    return this.family;
  }

  setListenBacklogSize(value: bigint): Result<void, ErrorCode> {
    if (value === 0n) return fail("invalid-argument");
    const backlog = value > BigInt(MAX_BACKLOG) ? MAX_BACKLOG : Number(value);
    switch (this.#state.kind) {
      case "default":
        this.#backlog = backlog;
        return ok(undefined);
      case "listening":
        return setOption<TcpOptions, "listenBacklogSize">(
          this.#backend,
          "listenBacklogSize",
          BigInt(backlog),
        );
      case "closed":
        return { tag: "err", val: this.#state.code };
      default:
        return fail("invalid-state");
    }
  }

  #get<K extends keyof TcpOptions>(name: K, valid: (v: unknown) => boolean) {
    if (this.#state.kind === "closed") return { tag: "err" as const, val: this.#state.code };
    return getOption<TcpOptions, K>(this.#backend, name, valid);
  }

  #set<K extends keyof TcpOptions>(name: K, value: TcpOptions[K], positive: boolean) {
    if (positive && (value === 0 || value === 0n)) return fail("invalid-argument");
    if (this.#state.kind === "closed") return { tag: "err" as const, val: this.#state.code };
    return setOption<TcpOptions, K>(this.#backend, name, value);
  }

  getKeepAliveEnabled() {
    return this.#get("keepAliveEnabled", isBool);
  }
  setKeepAliveEnabled(value: boolean) {
    return this.#set("keepAliveEnabled", value, false);
  }
  getKeepAliveIdleTime() {
    return this.#get("keepAliveIdleTime", isU64);
  }
  setKeepAliveIdleTime(value: bigint) {
    return this.#set("keepAliveIdleTime", value, true);
  }
  getKeepAliveInterval() {
    return this.#get("keepAliveInterval", isU64);
  }
  setKeepAliveInterval(value: bigint) {
    return this.#set("keepAliveInterval", value, true);
  }
  getKeepAliveCount() {
    return this.#get("keepAliveCount", isU32);
  }
  setKeepAliveCount(value: number) {
    return this.#set("keepAliveCount", value, true);
  }
  getHopLimit() {
    return this.#get("hopLimit", isU8);
  }
  setHopLimit(value: number) {
    return this.#set("hopLimit", value, true);
  }
  getReceiveBufferSize() {
    return this.#get("receiveBufferSize", isU64);
  }
  setReceiveBufferSize(value: bigint) {
    return this.#set("receiveBufferSize", value, true);
  }
  getSendBufferSize() {
    return this.#get("sendBufferSize", isU64);
  }
  setSendBufferSize(value: bigint) {
    return this.#set("sendBufferSize", value, true);
  }
}

// ---------------------------------------------------------------------------------------------
// UDP

/** `wasi:sockets/types#udp-socket`. Peer association and receive filtering are host state. */
export class UdpSocket extends SocketResource {
  #bound = false;
  #remote: Address | undefined;

  constructor(
    readonly access: SocketAccess,
    readonly family: IpAddressFamily,
    readonly shared: Shared<UdpBackend>,
  ) {
    super();
  }

  protected close(): void {
    this.shared.release();
  }

  get #backend(): UdpBackend {
    return this.shared.backend;
  }

  get #grants() {
    return this.access.grants;
  }

  bind(local: IpSocketAddress): Result<void, ErrorCode> {
    if (this.#bound) return fail("invalid-state");
    const address = fromWit(local)!;
    if (!hasFamily(address, this.family)) return fail("invalid-argument");
    if (!this.#grants.allows(address, "udp-bind")) return fail("access-denied");
    return this.#bind(address);
  }

  #bind(address: Address): Result<void, ErrorCode> {
    const backend = this.#backend;
    if (!backend.bind) return fail("not-supported");
    const bound = attempt(() => backend.bind!(toWit(address)));
    if (bound.tag === "ok") this.#bound = true;
    return bound;
  }

  connect(remote: IpSocketAddress): Result<void, ErrorCode> {
    const address = fromWit(remote)!;
    if (!hasFamily(address, this.family) || !isRemote(address)) return fail("invalid-argument");
    const implicit = unspecified(this.family);
    if (!this.#bound && !this.#grants.allows(implicit, "udp-bind")) return fail("access-denied");
    if (!this.#grants.allows(address, "udp-send") || !this.#grants.allows(address, "udp-receive"))
      return fail("access-denied");
    if (!this.#bound) {
      const bound = this.#bind(implicit);
      if (bound.tag === "err") return bound;
    }
    this.#remote = address;
    return ok(undefined);
  }

  disconnect(): Result<void, ErrorCode> {
    if (!this.#remote) return fail("invalid-state");
    this.#remote = undefined;
    return ok(undefined);
  }

  async send(
    data: Uint8Array,
    remote: IpSocketAddress | undefined,
    call: StandardCall,
  ): Promise<Result<void, ErrorCode>> {
    if (data.byteLength > MAX_DATAGRAM) return fail("datagram-too-large");
    let target: Address;
    if (remote !== undefined) {
      target = fromWit(remote)!;
      if (!isRemote(target) || !hasFamily(target, this.family)) return fail("invalid-argument");
      if (this.#remote && key(this.#remote) !== key(target)) return fail("invalid-argument");
    } else if (this.#remote) target = this.#remote;
    else return fail("invalid-argument");
    if (!this.#bound && !this.#grants.allows(unspecified(this.family), "udp-bind"))
      return fail("access-denied");
    if (!this.#grants.allows(target, "udp-send")) return fail("access-denied");
    const abort = new AbortController();
    const stop = () => abort.abort();
    call.signal?.addEventListener("abort", stop, { once: true });
    try {
      await abortable(
        call.host.physical(call.signal, () =>
          this.#backend.send(data.slice(), toWit(target), abort.signal),
        ),
        call.signal,
      );
      this.#bound = true;
      return ok(undefined);
    } catch (error) {
      if (call.signal?.aborted) throw cancelled();
      return { tag: "err", val: socketCode(error) };
    } finally {
      call.signal?.removeEventListener("abort", stop);
    }
  }

  async receive(call: StandardCall): Promise<Result<[Uint8Array, IpSocketAddress], ErrorCode>> {
    if (!this.#bound) return fail("invalid-state");
    const abort = new AbortController();
    const stop = () => abort.abort();
    call.signal?.addEventListener("abort", stop, { once: true });
    try {
      for (;;) {
        // A datagram arriving after cancellation is dropped, like network loss.
        const datagram = await abortable(
          call.host.physical(call.signal, () => this.#backend.receive(abort.signal)),
          call.signal,
        );
        const sender = provided(datagram?.remote);
        if (!(datagram.data instanceof Uint8Array) || datagram.data.byteLength > MAX_DATAGRAM)
          throw new Error("invalid provider datagram");
        // Datagrams from ungranted or unconnected peers are dropped, never delivered.
        if (this.#remote && key(this.#remote) !== key(sender)) continue;
        if (!this.#grants.allows(sender, "udp-receive")) continue;
        return ok([datagram.data.slice(), toWit(sender)] as [Uint8Array, IpSocketAddress]);
      }
    } catch (error) {
      if (call.signal?.aborted) throw cancelled();
      return { tag: "err", val: socketCode(error) };
    } finally {
      call.signal?.removeEventListener("abort", stop);
    }
  }

  getLocalAddress(): Result<IpSocketAddress, ErrorCode> {
    return this.#bound ? localAddress(this.#backend.local) : fail("invalid-state");
  }

  getRemoteAddress(): Result<IpSocketAddress, ErrorCode> {
    return this.#remote ? ok(toWit(this.#remote)) : fail("invalid-state");
  }

  getAddressFamily(): IpAddressFamily {
    return this.family;
  }

  #set<K extends keyof UdpOptions>(name: K, value: UdpOptions[K]) {
    if (value === 0 || value === 0n) return fail("invalid-argument");
    return setOption<UdpOptions, K>(this.#backend, name, value);
  }

  getUnicastHopLimit() {
    return getOption<UdpOptions, "unicastHopLimit">(this.#backend, "unicastHopLimit", isU8);
  }
  setUnicastHopLimit(value: number) {
    return this.#set("unicastHopLimit", value);
  }
  getReceiveBufferSize() {
    return getOption<UdpOptions, "receiveBufferSize">(this.#backend, "receiveBufferSize", isU64);
  }
  setReceiveBufferSize(value: bigint) {
    return this.#set("receiveBufferSize", value);
  }
  getSendBufferSize() {
    return getOption<UdpOptions, "sendBufferSize">(this.#backend, "sendBufferSize", isU64);
  }
  setSendBufferSize(value: bigint) {
    return this.#set("sendBufferSize", value);
  }
}

// ---------------------------------------------------------------------------------------------
// name lookup and binding

/** `resolve-addresses`: grant first (upstream order), then `url::Host::parse`, then provider. */
async function resolveAddresses(
  access: SocketAccess | undefined,
  name: string,
  call: StandardCall,
): Promise<Result<IpAddress[], LookupErrorCode>> {
  const failure = (tag: LookupErrorCode["tag"]) => ({
    tag: "err" as const,
    val: { tag } as LookupErrorCode,
  });
  if (!access?.grants.resolve) return failure("access-denied");
  const parsed = parseHostName(name);
  if (!parsed) return failure("invalid-argument");
  if ("ip" in parsed) return ok([parsed.ip]);
  const resolve = access.provider?.resolve;
  if (!resolve) return failure("permanent-resolver-failure");
  const abort = new AbortController();
  const stop = () => abort.abort();
  call.signal?.addEventListener("abort", stop, { once: true });
  try {
    const found = await abortable(
      call.host.physical(call.signal, () =>
        resolve.call(access.provider, parsed.domain, abort.signal),
      ),
      call.signal,
    );
    if (!Array.isArray(found)) throw new Error("invalid provider result");
    const addresses = found.map(checkedIpAddress);
    if (addresses.some((address) => address === undefined))
      throw new Error("invalid provider address");
    // The standard never succeeds with no addresses.
    if (!addresses.length) return failure("name-unresolvable");
    return ok(addresses as IpAddress[]);
  } catch (error) {
    if (call.signal?.aborted) throw cancelled();
    return { tag: "err", val: lookupCode(error) };
  } finally {
    call.signal?.removeEventListener("abort", stop);
  }
}

/** `create` for one protocol: grant, then capability, then the provider's synchronous create. */
function create<B extends { close(): Promise<void> }, S>(
  access: SocketAccess | undefined,
  allowed: boolean,
  factory: { create(family: IpAddressFamily): B } | undefined,
  family: IpAddressFamily,
  host: StandardServices,
  make: (access: SocketAccess, shared: Shared<B>) => S,
): Result<S, ErrorCode> {
  if (!access || !allowed) return fail("access-denied");
  if (!factory) return fail("not-supported");
  const created = attempt(() => factory.create(family));
  if (created.tag === "err") return created;
  if (typeof created.val?.close !== "function")
    return { tag: "err", val: { tag: "other", val: "invalid provider socket" } };
  return ok(make(access, new Shared(host, created.val)));
}

/** The constructors and free function behind the generated binding, keyed by WIT role. */
export function socketStatics(bindings: SocketBindings) {
  const types = bindings.types;
  return {
    tcpCreate: (call: StandardCall, family: IpAddressFamily) =>
      create(
        types,
        types?.grants.tcp ?? false,
        types?.provider?.tcp,
        family,
        call.host,
        (access, shared) => new TcpSocket(access, family, shared),
      ),
    udpCreate: (call: StandardCall, family: IpAddressFamily) =>
      create(
        types,
        types?.grants.udp ?? false,
        types?.provider?.udp,
        family,
        call.host,
        (access, shared) => new UdpSocket(access, family, shared),
      ),
    resolveAddresses: (call: StandardCall, name: string) =>
      resolveAddresses(bindings.lookup, name, call),
  };
}

/** Every generated `wasi:sockets` function, keyed for the standard host. */
export function socketMethods(bindings: SocketBindings): Record<string, Implementation> {
  const statics = socketStatics(bindings);
  return {
    ...bindResource(TYPES_INTERFACE, "tcp-socket", TcpSocket.prototype, {
      create: statics.tcpCreate,
    }),
    ...bindResource(TYPES_INTERFACE, "udp-socket", UdpSocket.prototype, {
      create: statics.udpCreate,
    }),
    [`${LOOKUP_INTERFACE}#resolveAddresses`]: (call, [name]: [string]) =>
      statics.resolveAddresses(call, name),
  };
}
