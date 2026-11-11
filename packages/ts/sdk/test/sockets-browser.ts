// Browser `wasi:sockets` acceptance: the authored socket Orb runs in the shared Wasm interpreter
// against explicit, typed injected providers. Browsers have no raw sockets, so the providers here
// are test-only in-memory networks; nothing in the product emulates a network.
import type { IpSocketAddress } from "wasi:sockets/types@0.3.0";
import {
  createHost,
  NameLookupError,
  SocketProviderError,
  type SocketProvider,
  type TcpBackend,
  type TcpConnection,
  type TcpListener,
  type UdpBackend,
} from "../src/browser.ts";

const CHUNK = 64 << 10;
const SIZE = 3 * (1 << 20);
const PROBES = "example:sockets/probes@1.0.0";
/** The fixture's block pattern: one value per 64 KiB block. */
const byte = (position: number, seed: number) => (Math.floor(position / CHUNK) + seed) % 251;
const loopback = (port: number): IpSocketAddress => ({
  tag: "ipv4",
  val: { port, address: [127, 0, 0, 1] },
});
const portOf = (address: IpSocketAddress | undefined) => address?.val.port ?? 0;
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function same(actual: unknown, expected: unknown, what: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected))
    throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

/** One direction of a connection, as the provider physically observes it. */
interface Direction {
  label: string;
  bytes: number;
  /** `fin` (writer closed), `aborted`, or `teardown` (the owning backend closed it). */
  writer: "open" | "fin" | "aborted" | "teardown";
  /** `end` (read to completion), `cancelled`, `errored`, or `teardown`. */
  reader: "open" | "end" | "cancelled" | "errored" | "teardown";
}

/** Provider calls, live backends and connection directions of one injected network. */
class Ledger {
  readonly calls: Record<string, number> = {};
  readonly live = new Set<string>();
  readonly directions: Direction[] = [];
  readonly failures: string[] = [];
  readonly lookups: string[] = [];
  created = 0;
  closed = 0;

  count(name: string): void {
    this.calls[name] = (this.calls[name] ?? 0) + 1;
  }

  find(label: string): Direction {
    const found = this.directions.find((direction) => direction.label === label);
    if (!found) throw new Error(`No connection direction ${label}`);
    return found;
  }

  /** Every backend closed and every direction ended physically. */
  settled(what: string): void {
    if (this.failures.length) throw new Error(`${what}: ${this.failures.join("; ")}`);
    if (this.live.size || this.created !== this.closed)
      throw new Error(`${what}: live backends ${[...this.live].join(", ")}`);
    for (const direction of this.directions)
      if (direction.writer === "open" || direction.reader === "open")
        throw new Error(`${what}: ${direction.label} is still open`);
  }
}

/** A one-way pipe whose two ends report to the ledger; `seed` verifies every byte. */
function direction(ledger: Ledger, label: string, seed?: number) {
  const state: Direction = { label, bytes: 0, writer: "open", reader: "open" };
  ledger.directions.push(state);
  const pipe = new TransformStream<Uint8Array, Uint8Array>();
  const writer = pipe.writable.getWriter();
  const reader = pipe.readable.getReader();
  const writable = new WritableStream<Uint8Array>({
    async write(chunk) {
      if (seed !== undefined)
        for (let index = 0; index < chunk.length; index++)
          if (chunk[index] !== byte(state.bytes + index, seed)) {
            ledger.failures.push(`${label} byte ${state.bytes + index} differs`);
            break;
          }
      state.bytes += chunk.length;
      await writer.write(chunk);
    },
    async close() {
      state.writer = "fin";
      await writer.close();
    },
    async abort(reason) {
      state.writer = "aborted";
      await writer.abort(reason);
    },
  });
  const readable = new ReadableStream<Uint8Array>(
    {
      async pull(controller) {
        try {
          const next = await reader.read();
          if (next.done) {
            if (state.reader === "open") state.reader = "end";
            controller.close();
          } else controller.enqueue(next.value);
        } catch (error) {
          if (state.reader === "open") state.reader = "errored";
          controller.error(error);
        }
      },
      async cancel(reason) {
        if (state.reader === "open") state.reader = "cancelled";
        await reader.cancel(reason);
      },
    },
    { highWaterMark: 0 },
  );
  /** Physical teardown by the backend that owns this end. */
  const teardown = async (side: "writer" | "reader") => {
    if (side === "writer" && state.writer === "open") {
      state.writer = "teardown";
      await writer.abort(new Error("socket closed")).catch(() => {});
    }
    if (side === "reader" && state.reader === "open") {
      state.reader = "teardown";
      await reader.cancel(new Error("socket closed")).catch(() => {});
    }
  };
  return { readable, writable, teardown };
}

type Pipe = ReturnType<typeof direction>;
/** A host-facing connection and the teardown its backend owes on close. */
interface Connected {
  connection: TcpConnection;
  close(): Promise<void>;
}

/** `simulated` peers have no backend of their own and go away with this connection. */
function connected(
  remote: IpSocketAddress,
  reads: Pipe,
  writes: Pipe,
  simulated = false,
): Connected {
  return {
    connection: { remote, readable: reads.readable, writable: writes.writable },
    close: async () => {
      await reads.teardown("reader");
      await writes.teardown("writer");
      if (!simulated) return;
      await reads.teardown("writer");
      await writes.teardown("reader");
    },
  };
}

/** Behaviour of a simulated remote endpoint the Orb may connect to. */
type Peer = (ledger: Ledger, signal: AbortSignal) => Promise<Connected>;

interface NetworkOptions {
  /** Synchronous `bind`/`listen`, UDP and name lookup; the relay has only `connect`. */
  full: boolean;
  peers: Record<number, Peer>;
  /** Byte patterns checked on loopback connections, by server port: [upload, download]. */
  verify?: Record<number, [number, number]>;
}

/** A typed injected socket provider over an in-memory network. */
function network(name: string, options: NetworkOptions) {
  const ledger = new Ledger();
  const listeners = new Map<
    number,
    { queue: { socket: TcpBackend; connection: TcpConnection }[]; wake?: () => void }
  >();
  const inboxes = new Map<
    number,
    { queue: { data: Uint8Array; remote: IpSocketAddress }[]; wake?: () => void }
  >();
  let ephemeral = 49152;
  let serial = 0;

  /** Wait for `ready` or abort; aborts reject so the host sees a settled promise. */
  async function waitFor(ready: () => boolean, box: { wake?: () => void }, signal: AbortSignal) {
    while (!ready()) {
      if (signal.aborted) throw new Error("aborted");
      await new Promise<void>((resolve) => {
        box.wake = resolve;
        signal.addEventListener("abort", () => resolve(), { once: true });
      });
    }
  }

  function tcp(initial?: Connected, local?: IpSocketAddress): TcpBackend {
    const id = `${name}:tcp#${++serial}`;
    ledger.created++;
    ledger.live.add(id);
    let current = initial;
    let listening: number | undefined;
    const backend: TcpBackend & { local: IpSocketAddress | undefined } = {
      local,
      async connect(remote, signal) {
        ledger.count("tcp.connect");
        const port = portOf(remote);
        backend.local ??= loopback(ephemeral++);
        const server = listeners.get(port);
        if (server) {
          const [up, down] = options.verify?.[port] ?? [];
          const upload = direction(ledger, `${port}:upload`, up);
          const download = direction(ledger, `${port}:download`, down);
          const accepted = connected(backend.local, upload, download);
          server.queue.push({
            socket: tcp(accepted, loopback(port)),
            connection: accepted.connection,
          });
          server.wake?.();
          current = connected(remote, download, upload);
          return current.connection;
        }
        const peer = options.peers[port];
        if (!peer) throw new SocketProviderError({ tag: "connection-refused" });
        current = await peer(ledger, signal);
        return current.connection;
      },
      async close() {
        ledger.count("tcp.close");
        if (listening !== undefined) listeners.delete(listening);
        await current?.close();
        ledger.closed++;
        ledger.live.delete(id);
      },
    };
    if (!options.full) return backend;
    backend.bind = (address) => {
      ledger.count("tcp.bind");
      const port = portOf(address);
      if (port && (listeners.has(port) || inboxes.has(port)))
        throw new SocketProviderError({ tag: "address-in-use" });
      backend.local = loopback(port || ephemeral++);
    };
    backend.listen = (backlog): TcpListener => {
      ledger.count("tcp.listen");
      if (backlog < 1) throw new SocketProviderError({ tag: "invalid-argument" });
      backend.local ??= loopback(ephemeral++);
      const port = portOf(backend.local);
      const box: { queue: { socket: TcpBackend; connection: TcpConnection }[]; wake?: () => void } =
        { queue: [] };
      listeners.set(port, box);
      listening = port;
      return {
        async accept(signal) {
          ledger.count("tcp.accept");
          await waitFor(() => box.queue.length > 0, box, signal);
          return box.queue.shift()!;
        },
      };
    };
    return backend;
  }

  function udp(): UdpBackend {
    const id = `${name}:udp#${++serial}`;
    ledger.created++;
    ledger.live.add(id);
    const box: { queue: { data: Uint8Array; remote: IpSocketAddress }[]; wake?: () => void } = {
      queue: [],
    };
    const backend: UdpBackend & { local: IpSocketAddress | undefined } = {
      local: undefined,
      bind(address) {
        ledger.count("udp.bind");
        const port = portOf(address) || ephemeral++;
        if (inboxes.has(port)) throw new SocketProviderError({ tag: "address-in-use" });
        backend.local = loopback(port);
        inboxes.set(port, box);
      },
      async send(data, remote) {
        ledger.count("udp.send");
        // The echo peer first sends from an address the Orb was never granted.
        const port = portOf(remote);
        if (port === 7101) {
          box.queue.push({ data: Uint8Array.of(66), remote: loopback(7102) });
          box.queue.push({ data: data.slice().reverse(), remote: loopback(port) });
          box.wake?.();
        }
      },
      async receive(signal) {
        ledger.count("udp.receive");
        await waitFor(() => box.queue.length > 0, box, signal);
        return box.queue.shift()!;
      },
      async close() {
        ledger.count("udp.close");
        inboxes.delete(portOf(backend.local));
        ledger.closed++;
        ledger.live.delete(id);
      },
    };
    return backend;
  }

  const provider: SocketProvider = options.full
    ? {
        tcp: { create: () => (ledger.count("tcp.create"), tcp()) },
        udp: { create: () => (ledger.count("udp.create"), udp()) },
        async resolve(host) {
          ledger.count("resolve");
          ledger.lookups.push(host);
          if (host === "missing.example") return [];
          if (host === "busy.example")
            throw new NameLookupError({ tag: "temporary-resolver-failure" });
          if (host === "service.example")
            return [
              { tag: "ipv6", val: [0, 0, 0, 0, 0, 0xffff, 0x0a00, 0x0008] },
              { tag: "ipv4", val: [10, 0, 0, 9] },
            ];
          return [{ tag: "ipv4", val: [10, 0, 0, 7] }];
        },
      }
    : { tcp: { create: () => (ledger.count("tcp.create"), tcp()) } };
  return { provider, ledger };
}

/** A peer that accepts and stays silent; the Orb reads nothing from it. */
const silent: Peer = async (ledger) => {
  const download = direction(ledger, "silent:download");
  const upload = direction(ledger, "silent:upload");
  return connected(loopback(7002), download, upload, true);
};

/** A peer whose connection completes only as the Orb abandons it: a late value. */
const late: Peer = async (ledger, signal) => {
  await new Promise<void>((resolve) => {
    if (signal.aborted) resolve();
    signal.addEventListener("abort", () => resolve(), { once: true });
  });
  ledger.count("tcp.connect.aborted");
  await tick();
  const download = direction(ledger, "late:download");
  const upload = direction(ledger, "late:upload");
  return connected(loopback(7001), download, upload, true);
};

/** A relay endpoint for the connect-only provider. */
const relayed: Peer = async (ledger) => {
  const download = direction(ledger, "relay:download");
  const upload = direction(ledger, "relay:upload");
  return connected(loopback(7200), download, upload, true);
};

/**
 * Generated `wasi:sockets` imports in an authored TypeScript Orb, compiled to a source artifact,
 * run by the shared Wasm interpreter in Chrome, with exact per-Orb provider grants.
 */
export async function verifySocketScenario(engine?: URL): Promise<string> {
  const fixture = async (name: string) => {
    const response = await fetch(`/${name}.wasm`);
    if (!response.ok) throw new Error(`Missing fixture ${name}`);
    return new Uint8Array(await response.arrayBuffer());
  };
  const artifact = await fixture("sockets");
  const peerArtifact = await fixture("sockets-peer");
  const local = network("loopback", {
    full: true,
    peers: { 7001: late, 7002: silent },
    verify: { 7000: [1, 2] },
  });
  const relay = network("relay", { full: false, peers: { 7200: relayed } });
  const host = await createHost({
    isolated: {
      ...(engine ? { engine } : {}),
      providers: { network: { loopback: local.provider, relay: relay.provider } },
    },
  });
  const report: string[] = [];
  let probes, peer;
  try {
    await host.install({
      artifact,
      grants: {
        network: [
          {
            provider: "loopback",
            connect: ["127.0.0.1:7000", "127.0.0.1:7001", "127.0.0.1:7002"],
            listen: ["127.0.0.1:7000"],
            bind: ["127.0.0.1:7100"],
            datagrams: ["127.0.0.1:7101"],
            resolve: true,
          },
        ],
      },
    });
    await host.install({
      artifact: peerArtifact,
      grants: {
        network: [
          {
            provider: "relay",
            connect: ["127.0.0.1:7200"],
            datagrams: ["127.0.0.1:7201"],
            resolve: true,
          },
        ],
      },
    });
    probes = await host.get(`sockets::${PROBES}`, "main");
    peer = await host.get(`sockets-peer::${PROBES}`, "main");

    // TCP: listen yields an owned accepted socket; 3 MiB each way; the client half-closes.
    same(
      await probes.exchange(7000, SIZE),
      `true|true|${SIZE}|${SIZE}|ok|ok|ok|ok`,
      "TCP exchange",
    );
    same(
      [
        local.ledger.find("7000:upload"),
        local.ledger.find("7000:download"),
        local.ledger.calls["tcp.accept"],
      ],
      [
        { label: "7000:upload", bytes: SIZE, writer: "fin", reader: "end" },
        { label: "7000:download", bytes: SIZE, writer: "fin", reader: "end" },
        1,
      ],
      "TCP exchange physical ends and accept pulls",
    );
    // Server, client and accepted backends closed before the callback settled.
    local.ledger.settled("TCP exchange cleanup");
    report.push(`exchange ${SIZE} bytes each way, FIN both ways, one accept pull`);

    // A connect pending at callback end is aborted; its late connection is closed, unpublished.
    same(await probes.abandon(7001), "pending", "abandoned connect");
    same(
      [
        local.ledger.calls["tcp.connect.aborted"],
        local.ledger.find("late:download").reader,
        local.ledger.find("late:upload").writer,
      ],
      [1, "cancelled", "aborted"],
      "late connection discarded by the host",
    );
    report.push("abandoned connect: provider aborted, late connection cancelled and aborted");

    // Dropping a receive stream with a read pending cancels the provider read at once.
    same(await probes.idle(7002), "dropped|ok", "idle receive drop");
    same(local.ledger.find("silent:download").reader, "cancelled", "receive drop");
    same(local.ledger.find("silent:upload").writer, "teardown", "unsent direction on close");
    report.push("idle receive drop: pending read failed as dropped, provider read cancelled");

    // UDP: the ungranted sender is filtered, refusals never reach the provider.
    const sends = local.ledger.calls["udp.send"] ?? 0;
    same(
      await probes.datagrams(7100, 7101, 7102),
      "127.0.0.1:7100|3,2,1|127.0.0.1:7101|127.0.0.1:7101|5,4|access-denied|datagram-too-large",
      "UDP datagrams",
    );
    same(local.ledger.calls["udp.send"]! - sends, 2, "UDP sends reaching the provider");
    report.push("udp: granted peer echo, ungranted sender filtered, standard refusals");

    // Name lookup: literals locally, IDNA in ASCII, mapped IPv6 canonicalized, standard codes.
    same(
      await probes.lookup([
        "127.0.0.1",
        "bücher.example",
        "service.example",
        "missing.example",
        "busy.example",
        "bad name",
      ]),
      "127.0.0.1;10.0.0.7;10.0.0.8,10.0.0.9;name-unresolvable;temporary-resolver-failure;invalid-argument",
      "name lookup",
    );
    same(
      local.ledger.lookups,
      ["xn--bcher-kva.example", "service.example", "missing.example", "busy.example"],
      "names reaching the provider",
    );
    report.push("lookup: literal, IDNA, mapped IPv6, unresolvable, temporary, invalid");

    // Per-Orb grants: each Orb reaches only its own provider and only its granted addresses.
    const before = JSON.stringify(local.ledger.calls);
    same(relay.ledger.created, 0, "relay untouched by the loopback Orb");
    same(
      await peer.refusals(7200),
      "connect:ok bind:access-denied implicit:not-supported udp:not-supported lookup:permanent-resolver-failure state:invalid-state family:invalid-argument",
      "relay Orb outcomes",
    );
    same(
      await peer.refusals(7000),
      "connect:access-denied bind:access-denied implicit:not-supported udp:not-supported lookup:permanent-resolver-failure state:invalid-state family:invalid-argument",
      "relay Orb on the loopback Orb's address",
    );
    same(JSON.stringify(local.ledger.calls), before, "loopback untouched by the relay Orb");
    same(relay.ledger.calls["tcp.connect"], 1, "relay connects");
    same(
      await probes.refusals(7200),
      "connect:access-denied bind:access-denied implicit:ok udp:access-denied lookup:ok state:invalid-state family:invalid-argument",
      "loopback Orb on the relay Orb's address",
    );
    relay.ledger.settled("relay cleanup");
    report.push("per-Orb grants: exact provider and addresses for each Orb");
  } finally {
    probes?.release();
    peer?.release();
    await host.close();
  }
  // The abandoned socket was held by the guest until retirement; nothing outlives the host.
  local.ledger.settled("loopback retirement");
  relay.ledger.settled("relay retirement");
  const calls = JSON.stringify([local.ledger.calls, relay.ledger.calls]);
  await tick();
  same(JSON.stringify([local.ledger.calls, relay.ledger.calls]), calls, "calls after close");
  const diagnostics = host.diagnostics();
  for (const name of [
    "objects",
    "references",
    "roots",
    "calls",
    "resources",
    "tasks",
    "orbs",
    "values",
    "pendingJobs",
  ] as const)
    if (diagnostics[name] !== 0) throw new Error(`${name} survived socket retirement`);
  return `standard sockets: ${report.join("; ")}`;
}
