import type { Grants } from "../authoring/types.ts";
import type { TransportStream } from "../guest.ts";
import type { IORequest } from "./host-protocol.ts";
import { checkSignal } from "../runtime/signals.ts";

/** Standard Fetch implementations can be replaced by application-owned mocks or proxies. */
export type HttpProvider = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;

/** Browsers have no ambient TCP provider; an embedding may explicitly supply one. */
export interface SocketProvider {
  connect(ip: string, port: number, signal: AbortSignal): Promise<TransportStream>;
}

export interface BrowserNetworkProviders {
  http?: Readonly<Record<string, HttpProvider>>;
  network?: Readonly<Record<string, SocketProvider>>;
}

// Match wasmtime-wasi-http's default forbidden fields; the host also owns content length.
const framingHeaders = new Set([
  "connection",
  "keep-alive",
  "proxy-authenticate",
  "proxy-authorization",
  "proxy-connection",
  "transfer-encoding",
  "upgrade",
  "host",
  "http2-settings",
  "content-length",
]);

type Stream = {
  kind: "http" | "socket";
  signal: AbortSignal;
  detach(): void;
  read(): Promise<Uint8Array | null>;
  write?: (bytes: Uint8Array) => Promise<void>;
  close(): Promise<void>;
  reading: boolean;
  writing: boolean;
  pending: Uint8Array;
};

/** Own only this Orb's issued response/socket handles and granted provider selections. */
export class BrowserNetwork {
  readonly #streams = new Map<number, Stream>();
  #issued = 0;
  #connecting = 0;
  #disposed = false;

  constructor(
    readonly grants: Grants,
    readonly providers: BrowserNetworkProviders,
  ) {
    for (const grant of grants.http ?? []) {
      if (!grant.provider.trim() || !grant.origins.length)
        throw new Error("HTTP grants require a provider and at least one origin");
      for (const origin of grant.origins) {
        const url = new URL(origin);
        if (
          !["http:", "https:"].includes(url.protocol) ||
          !url.hostname ||
          url.username ||
          url.password ||
          url.href.includes("#") ||
          url.pathname !== "/" ||
          url.href.includes("?") ||
          url.hostname.includes("*")
        )
          throw new Error(
            "HTTP grants require exact http(s) origins without credentials, paths, queries or wildcards",
          );
      }
    }
  }

  /** Check grants before calling a host provider and keep every operation bound to its callback. */
  async execute(request: IORequest, signal?: AbortSignal): Promise<unknown> {
    if (request.operation === "http.close" || request.operation === "transport.close")
      return this.close(request.id, request.operation === "http.close" ? "http" : "socket");
    if (!signal) throw new Error("Network I/O requires an active callback");
    checkSignal(signal);
    if (this.#disposed) throw new Error("Network providers are closed");
    if (request.operation === "http.send" || request.operation === "transport.connect") {
      if (this.#streams.size + this.#connecting >= 16)
        throw new Error("Network stream limit reached");
      this.#connecting++;
      try {
        return request.operation === "http.send"
          ? await this.send(request, signal)
          : await this.connect(request.ip, request.port, signal);
      } finally {
        this.#connecting--;
      }
    }
    const kind = request.operation.startsWith("http.") ? "http" : "socket";
    if (!("id" in request)) throw new Error("Expected a network operation");
    const stream = this.#streams.get(request.id);
    if (!stream || stream.kind !== kind || stream.signal.aborted)
      throw new Error("Unknown network stream");
    if (request.operation === "http.read" || request.operation === "transport.read") {
      if (stream.reading) throw new Error("Network stream already has a reader");
      stream.reading = true;
      try {
        let bytes = stream.pending;
        while (bytes.length === 0) {
          const next = await stream.read();
          if (next === null) return null;
          if (!(next instanceof Uint8Array))
            throw new Error("Network provider returned invalid bytes");
          checkSignal(stream.signal);
          bytes = next;
        }
        stream.pending = bytes.subarray(16384);
        return Array.from(bytes.subarray(0, 16384));
      } finally {
        stream.reading = false;
      }
    }
    if (request.operation === "transport.write") {
      if (!stream.write || stream.writing) throw new Error("Network stream writer is unavailable");
      stream.writing = true;
      try {
        await stream.write(Uint8Array.from(request.bytes));
        return null;
      } finally {
        stream.writing = false;
      }
    }
    throw new Error("Expected a network operation");
  }

  /** Register a handle only while its originating callback still owns it. */
  private async register(
    stream: Omit<Stream, "detach" | "reading" | "writing" | "pending">,
  ): Promise<number> {
    if (this.#disposed || stream.signal.aborted) {
      await stream.close();
      checkSignal(stream.signal);
      throw new Error("Network providers are closed");
    }
    if (this.#issued === 0xffffffff) {
      await stream.close();
      throw new Error("Network identities exhausted");
    }
    const id = ++this.#issued;
    const abort = () => {
      void this.close(id, stream.kind).catch(() => {});
    };
    this.#streams.set(id, {
      ...stream,
      detach: () => stream.signal.removeEventListener("abort", abort),
      reading: false,
      writing: false,
      pending: new Uint8Array(),
    });
    stream.signal.addEventListener("abort", abort, { once: true });
    return id;
  }

  /** Delegate HTTP framing, DNS and TLS to the selected standard Fetch implementation. */
  private async send(request: Extract<IORequest, { operation: "http.send" }>, signal: AbortSignal) {
    const url = new URL(request.url);
    if (
      !["http:", "https:"].includes(url.protocol) ||
      url.username ||
      url.password ||
      url.href.includes("#")
    )
      throw new Error("HTTP requires an http(s) URL without credentials or fragments");
    const grant = this.grants.http?.find((grant) =>
      grant.origins.some((origin) => new URL(origin).origin === url.origin),
    );
    const provider = grant && this.providers.http?.[grant.provider];
    if (!provider) throw new Error("HTTP origin has no granted host provider");
    const headers = new Headers(request.headers as [string, string][]);
    for (const name of headers.keys())
      if (framingHeaders.has(name)) throw new Error(`HTTP header ${name} is set by the host`);
    const controller = new AbortController();
    const abort = () => controller.abort(signal.reason);
    signal.addEventListener("abort", abort, { once: true });
    let response: Response;
    try {
      response = await provider(url, {
        method: request.method,
        headers,
        ...(request.body.length ? { body: Uint8Array.from(request.body) } : {}),
        credentials: "omit",
        redirect: "error",
        signal: controller.signal,
      });
    } catch (error) {
      signal.removeEventListener("abort", abort);
      throw error;
    }
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      reader = response.body?.getReader();
    } catch (error) {
      signal.removeEventListener("abort", abort);
      controller.abort();
      throw error;
    }
    const id = await this.register({
      kind: "http",
      signal,
      async read() {
        const next = await reader?.read();
        return !next || next.done ? null : next.value;
      },
      async close() {
        signal.removeEventListener("abort", abort);
        controller.abort();
        try {
          await reader?.cancel();
        } finally {
          reader?.releaseLock();
        }
      },
    });
    return { id, status: response.status, headers: [...response.headers] };
  }

  /** Resolve an explicitly provided socket adapter; raw network authority never follows HTTP grants. */
  private async connect(ip: string, port: number, signal: AbortSignal) {
    const address = `${ip.includes(":") ? `[${ip}]` : ip}:${port}`;
    const grant = this.grants.network?.find((grant) => grant.connect?.includes(address));
    const provider = grant && this.providers.network?.[grant.provider];
    if (!provider) throw new Error("This browser host has no raw TCP provider for this grant");
    const stream = await provider.connect(ip, port, signal);
    return this.register({
      kind: "socket",
      signal,
      read: () => stream.read(),
      write: (bytes) => stream.write(bytes),
      close: () => stream.close(),
    });
  }

  /** Closing issued handles is idempotent, including cancellation cleanup. */
  private async close(id: number, kind: Stream["kind"]): Promise<null> {
    if (id <= 0 || id > this.#issued) throw new Error("Unknown network stream");
    const stream = this.#streams.get(id);
    if (stream) {
      if (stream.kind !== kind) throw new Error("Unknown network stream");
      this.#streams.delete(id);
      stream.detach();
      await stream.close();
    }
    return null;
  }

  /** Retire all handles after the guest's physical work has drained. */
  async dispose(): Promise<void> {
    this.#disposed = true;
    const results = await Promise.allSettled(
      [...this.#streams].map(([id, stream]) => this.close(id, stream.kind)),
    );
    const failures = results.filter((result) => result.status === "rejected");
    if (failures.length)
      throw new AggregateError(
        failures.map((result) => result.reason),
        "Network cleanup failed",
      );
  }
}
