import type { Grants } from "../authoring/types.ts";
import type { TransportStream } from "../guest.ts";
import type { IORequest } from "./host-protocol.ts";
import { checkSignal } from "../runtime/signals.ts";
import type { HttpProvider } from "./http-provider.ts";
import type { HttpClient } from "./standard-http.ts";

/** Browsers have no ambient TCP provider; an embedding may explicitly supply one. */
export interface SocketProvider {
  connect(ip: string, port: number, signal: AbortSignal): Promise<TransportStream>;
}

export interface BrowserNetworkProviders {
  http?: Readonly<Record<string, HttpProvider>>;
  network?: Readonly<Record<string, SocketProvider>>;
}

/** A raw socket issued to one callback (Phase 4b keeps this legacy carrier). */
type Stream = {
  signal: AbortSignal;
  detach(): void;
  read(): Promise<Uint8Array | null>;
  write?: (bytes: Uint8Array) => Promise<void>;
  close(): Promise<void>;
  reading: boolean;
  writing: boolean;
  pending: Uint8Array;
};

/**
 * Granted network selections of one Orb. HTTP is served only through standard `wasi:http`
 * (`provider`, checked by the standard host before any provider call); raw sockets remain the
 * legacy source carrier until Phase 4b.
 */
export class BrowserNetwork implements HttpClient {
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
          url.hostname.includes("*") ||
          url.port === "0"
        )
          throw new Error(
            "HTTP grants require exact http(s) origins without credentials, paths, queries, wildcards or port 0",
          );
      }
    }
  }

  /**
   * The provider granted for exactly `url`'s origin. Standard `wasi:http` sends call this before
   * any provider work; HTTP grants never follow raw socket grants or the reverse.
   */
  provider(url: URL): HttpProvider | undefined {
    if (
      this.#disposed ||
      (url.protocol !== "http:" && url.protocol !== "https:") ||
      url.username ||
      url.password
    )
      return undefined;
    const grant = this.grants.http?.find((grant) =>
      grant.origins.some((origin) => new URL(origin).origin === url.origin),
    );
    return grant && this.providers.http?.[grant.provider];
  }

  /** Check socket grants before calling a provider and keep every operation bound to its callback. */
  async execute(request: IORequest, signal?: AbortSignal): Promise<unknown> {
    if (request.operation === "transport.close") return this.close(request.id);
    if (!signal) throw new Error("Network I/O requires an active callback");
    checkSignal(signal);
    if (this.#disposed) throw new Error("Network providers are closed");
    if (request.operation === "transport.connect") {
      if (this.#streams.size + this.#connecting >= 16)
        throw new Error("Network stream limit reached");
      this.#connecting++;
      try {
        return await this.connect(request.ip, request.port, signal);
      } finally {
        this.#connecting--;
      }
    }
    if (request.operation !== "transport.read" && request.operation !== "transport.write")
      throw new Error("Expected a network operation");
    const stream = this.#streams.get(request.id);
    if (!stream || stream.signal.aborted) throw new Error("Unknown network stream");
    if (request.operation === "transport.read") {
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
      void this.close(id).catch(() => {});
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

  /** Resolve an explicitly provided socket adapter; raw network authority never follows HTTP grants. */
  private async connect(ip: string, port: number, signal: AbortSignal) {
    const address = `${ip.includes(":") ? `[${ip}]` : ip}:${port}`;
    const grant = this.grants.network?.find((grant) => grant.connect?.includes(address));
    const provider = grant && this.providers.network?.[grant.provider];
    if (!provider) throw new Error("This browser host has no raw TCP provider for this grant");
    const stream = await provider.connect(ip, port, signal);
    return this.register({
      signal,
      read: () => stream.read(),
      write: (bytes) => stream.write(bytes),
      close: () => stream.close(),
    });
  }

  /** Closing issued handles is idempotent, including cancellation cleanup. */
  private async close(id: number): Promise<null> {
    if (id <= 0 || id > this.#issued) throw new Error("Unknown network stream");
    const stream = this.#streams.get(id);
    if (stream) {
      this.#streams.delete(id);
      stream.detach();
      await stream.close();
    }
    return null;
  }

  /** Retire all handles after the guest's physical work has drained. */
  async dispose(): Promise<void> {
    this.#disposed = true;
    const results = await Promise.allSettled([...this.#streams.keys()].map((id) => this.close(id)));
    const failures = results.filter((result) => result.status === "rejected");
    if (failures.length)
      throw new AggregateError(
        failures.map((result) => result.reason),
        "Network cleanup failed",
      );
  }
}
