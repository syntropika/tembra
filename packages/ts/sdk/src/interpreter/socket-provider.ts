/**
 * The injectable host transport behind standard `wasi:sockets` and `wasi:sockets/ip-name-lookup`
 * in the browser. Browsers have no raw sockets: an embedder supplies a provider, and every
 * member is optional, so a provider may support only a subset. A missing member is a standard
 * `not-supported` (`permanent-resolver-failure` for name lookup); it is never emulated.
 *
 * The standard host owns permissions, references and physical accounting. It checks the exact
 * grant before calling a provider, and validates and copies everything a provider returns.
 * Every promise below is physical work. The host tracks it until it settles, even after the
 * guest's call was cancelled, so a provider must settle it once the work has really stopped,
 * including releasing anything it produced late.
 */
import type {
  Duration,
  ErrorCode as SocketErrorCode,
  IpAddress,
  IpAddressFamily,
  IpSocketAddress,
} from "wasi:sockets/types@0.3.0";
import type { ErrorCode as NameLookupErrorCode } from "wasi:sockets/ip-name-lookup@0.3.0";

/** One named network provider, selected by the Orb's `wasi:sockets` import binding. */
export interface SocketProvider {
  /** Synchronous: `tcp-socket.create`. Throw `SocketProviderError` for an unsupported family. */
  readonly tcp?: { create(family: IpAddressFamily): TcpBackend };
  /** Synchronous: `udp-socket.create`. */
  readonly udp?: { create(family: IpAddressFamily): UdpBackend };
  /** `resolve-addresses` for an ASCII (IDNA) domain name; IP literals never reach it. */
  resolve?(name: string, signal: AbortSignal): Promise<readonly IpAddress[]>;
}

/** TCP socket options, by their standard getter and setter names. */
export interface TcpOptions {
  keepAliveEnabled: boolean;
  keepAliveIdleTime: Duration;
  keepAliveInterval: Duration;
  keepAliveCount: number;
  hopLimit: number;
  receiveBufferSize: bigint;
  sendBufferSize: bigint;
  /** Only after `listen`; before it, the host keeps the backlog as upstream Wasmtime does. */
  listenBacklogSize: bigint;
}

/** UDP socket options, by their standard getter and setter names. */
export interface UdpOptions {
  unicastHopLimit: number;
  receiveBufferSize: bigint;
  sendBufferSize: bigint;
}

/**
 * Real option access. Getting and setting are supported independently; an absent method, or a
 * thrown `not-supported`, is reported as `not-supported`. Setters receive values the host has
 * already validated as WASI requires (for example, never 0 where 0 is invalid).
 */
export interface SocketOptions<O> {
  getOption?<K extends keyof O>(name: K): O[K];
  setOption?<K extends keyof O>(name: K, value: O[K]): void;
}

/** A provider-owned TCP socket. The host closes it once its last reference is released. */
export interface TcpBackend extends SocketOptions<TcpOptions> {
  /** The actual bound local address, or `undefined` before any explicit or implicit bind. */
  readonly local: IpSocketAddress | undefined;
  /** Synchronous explicit bind; returns only once bound. Absent means `not-supported`. */
  bind?(local: IpSocketAddress): void;
  /** Synchronous; binds implicitly when unbound. Absent means `not-supported`. */
  listen?(backlog: number): TcpListener;
  /** Binds implicitly when unbound. */
  connect(remote: IpSocketAddress, signal: AbortSignal): Promise<TcpConnection>;
  /** The last reference was released; the promise is the physical acknowledgement. */
  close(): Promise<void>;
}

/** The accept side of a listening `TcpBackend`; listening ends when that backend closes. */
export interface TcpListener {
  /**
   * Accept one connection. The host keeps at most one call pending and aborts `signal` when
   * nobody will take its result; a connection delivered anyway is closed by the host.
   */
  accept(signal: AbortSignal): Promise<{ socket: TcpBackend; connection: TcpConnection }>;
}

/** The byte directions of a connected socket. Each promise is a physical acknowledgement. */
export interface TcpConnection {
  readonly remote: IpSocketAddress;
  /** Read as the guest reads. `cancel()` is a read shutdown; empty chunks are allowed. */
  readonly readable: ReadableStream<Uint8Array>;
  /** `close()` is a write shutdown after the last write; `abort()` stops a failed send. */
  readonly writable: WritableStream<Uint8Array>;
}

/** A provider-owned UDP socket. */
export interface UdpBackend extends SocketOptions<UdpOptions> {
  readonly local: IpSocketAddress | undefined;
  /** Synchronous explicit bind, also used for the implicit bind of `connect`. */
  bind?(local: IpSocketAddress): void;
  /** Binds implicitly when unbound; resolves once the datagram was handed to the network. */
  send(data: Uint8Array, remote: IpSocketAddress, signal: AbortSignal): Promise<void>;
  /** The next datagram from any peer; the host filters peers by grant and connection. */
  receive(signal: AbortSignal): Promise<{ data: Uint8Array; remote: IpSocketAddress }>;
  close(): Promise<void>;
}

/** A provider failure carrying a standard socket `error-code`. */
export class SocketProviderError extends Error {
  constructor(
    readonly code: SocketErrorCode,
    message = `socket provider failed: ${code.tag}`,
  ) {
    super(message);
    this.name = "SocketProviderError";
  }
}

/** A provider failure carrying a standard name lookup `error-code`. */
export class NameLookupError extends Error {
  constructor(
    readonly code: NameLookupErrorCode,
    message = `name lookup failed: ${code.tag}`,
  ) {
    super(message);
    this.name = "NameLookupError";
  }
}
