/** Real compiled WASI sockets, over explicit typed browser providers with physical accounting. */
import type { IpSocketAddress } from "wasi:sockets/types@0.3.0";
import {
  createHost,
  NameLookupError,
  type PreparedComponent,
  type SocketProvider,
  SocketProviderError,
  type TcpBackend,
  type TcpConnection,
  type TcpOptions,
  type UdpBackend,
  type UdpOptions,
} from "../src/browser.ts";
import type { Host } from "../src/host.ts";

type Reference = { release(): void; [method: string]: any };
const address = (port: number): IpSocketAddress => ({
  tag: "ipv4",
  val: { address: [127, 0, 0, 1], port },
});
const portOf = (value: IpSocketAddress | undefined) => value?.val.port ?? 0;
const encode = (text: string) => new TextEncoder().encode(text);
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function same(actual: unknown, expected: unknown, label: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected))
    throw new Error(
      `${label}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
    );
}

function ok(value: { tag: string; val?: unknown }, label: string): any {
  if (value?.tag !== "ok") throw new Error(`${label}: ${JSON.stringify(value)}`);
  return value.val;
}

function error(value: { tag: string; val?: unknown }, part: string, label: string): void {
  if (value?.tag !== "err" || !String(value.val).includes(part))
    throw new Error(`${label}: expected ${part}, got ${JSON.stringify(value)}`);
}

async function until(ready: () => boolean, label: string): Promise<void> {
  const deadline = performance.now() + 5000;
  while (!ready()) {
    if (performance.now() >= deadline) throw new Error(`Never observed ${label}`);
    await tick();
  }
}

/** Resolve a provider operation only after cancellation physically stops it. */
async function stopped(signal: AbortSignal): Promise<never> {
  if (!signal.aborted)
    await new Promise<void>((resolve) =>
      signal.addEventListener("abort", () => resolve(), { once: true }),
    );
  throw new SocketProviderError({ tag: "connection-aborted" });
}

/** A deterministic provider, not a substitute browser network implementation. */
function network() {
  const calls: Record<string, number> = {};
  const live = new Set<number>();
  const lookups: string[] = [];
  const received: number[] = [];
  const count = (name: string) => (calls[name] = (calls[name] ?? 0) + 1);
  let serial = 0;

  function connection(port: number, text?: string, eof = false): TcpConnection {
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    let ended = false;
    return {
      remote: address(port),
      readable: new ReadableStream<Uint8Array>(
        {
          start(value) {
            controller = value;
            if (text !== undefined) value.enqueue(encode(text));
            if (eof) {
              ended = true;
              value.close();
            }
          },
          pull() {
            count("tcp.read");
          },
          cancel() {
            count("tcp.read.cancel");
            ended = true;
          },
        },
        { highWaterMark: 0 },
      ),
      writable: new WritableStream<Uint8Array>({
        async write(bytes, controller) {
          count("tcp.write");
          received.push(...bytes);
          if (port === 7407) {
            try {
              await stopped(controller.signal);
            } finally {
              count("tcp.write.aborted");
            }
          }
        },
        close() {
          count("tcp.write.close");
          if (!ended) {
            ended = true;
            controller.close();
          }
        },
        abort() {
          count("tcp.write.abort");
          if (!ended) {
            ended = true;
            controller.close();
          }
        },
      }),
    };
  }

  function tcp(): TcpBackend {
    const id = ++serial;
    live.add(id);
    let current: TcpConnection | undefined;
    const options: TcpOptions = {
      keepAliveEnabled: false,
      keepAliveIdleTime: 1n,
      keepAliveInterval: 1n,
      keepAliveCount: 1,
      hopLimit: 1,
      receiveBufferSize: 1n,
      sendBufferSize: 1n,
      listenBacklogSize: 128n,
    };
    const backend: TcpBackend & { local: IpSocketAddress | undefined } = {
      local: undefined,
      getOption(name) {
        count(`tcp.get.${name}`);
        return options[name];
      },
      setOption(name, value) {
        count(`tcp.set.${name}`);
        options[name] = value;
      },
      bind(value) {
        count("tcp.bind");
        backend.local = value;
      },
      listen(backlog) {
        count("tcp.listen");
        calls["tcp.backlog"] = backlog;
        let index = 0;
        return {
          async accept(signal) {
            count("tcp.accept");
            if (portOf(backend.local) === 7400) {
              const accepted = tcp();
              const remote = connection(7500 + index, `client-${index++}`, true);
              return { socket: accepted, connection: remote };
            }
            if (portOf(backend.local) === 7408) {
              await stopped(signal).catch(() => {});
              await tick();
              count("tcp.accept.late");
              return { socket: tcp(), connection: connection(7509) };
            }
            try {
              return await stopped(signal);
            } finally {
              count("tcp.accept.aborted");
            }
          },
        };
      },
      async connect(value, signal) {
        count("tcp.connect");
        backend.local = address(49152 + id);
        const port = portOf(value);
        if (port === 7499) {
          try {
            return await stopped(signal);
          } finally {
            count("tcp.connect.aborted");
          }
        }
        if (port === 7409) throw new SocketProviderError({ tag: "connection-refused" });
        current = connection(
          port,
          port === 7401 ? "hello" : port === 7402 ? "abc" : undefined,
          port === 7402,
        );
        return current;
      },
      async close() {
        count("tcp.close");
        if (!live.delete(id)) throw new Error("TCP backend closed twice");
        if (current) {
          if (!current.readable.locked) await current.readable.cancel();
          if (!current.writable.locked) await current.writable.abort();
        }
      },
    };
    return backend;
  }

  function udp(): UdpBackend {
    const id = ++serial;
    live.add(id);
    let pending: { data: Uint8Array; remote: IpSocketAddress } | undefined;
    const options: UdpOptions = { unicastHopLimit: 1, receiveBufferSize: 1n, sendBufferSize: 1n };
    const backend: UdpBackend & { local: IpSocketAddress | undefined } = {
      local: undefined,
      getOption(name) {
        count(`udp.get.${name}`);
        return options[name];
      },
      setOption(name, value) {
        count(`udp.set.${name}`);
        options[name] = value;
      },
      bind(value) {
        count("udp.bind");
        backend.local = value;
      },
      async send(data, remote) {
        count("udp.send");
        if (portOf(remote) !== 7406)
          pending = { data: encode(`echo:${new TextDecoder().decode(data)}`), remote };
      },
      async receive(signal) {
        count("udp.receive");
        if (!pending) {
          try {
            return await stopped(signal);
          } finally {
            count("udp.receive.aborted");
          }
        }
        const value = pending;
        pending = undefined;
        return value;
      },
      async close() {
        count("udp.close");
        if (!live.delete(id)) throw new Error("UDP backend closed twice");
      },
    };
    return backend;
  }

  const provider: SocketProvider = {
    tcp: { create: () => (count("tcp.create"), tcp()) },
    udp: { create: () => (count("udp.create"), udp()) },
    async resolve(name, signal) {
      count("dns.resolve");
      lookups.push(name);
      if (name === "pending.example") {
        try {
          return await stopped(signal);
        } finally {
          count("dns.resolve.aborted");
        }
      }
      if (name === "busy.example") throw new NameLookupError({ tag: "temporary-resolver-failure" });
      if (name === "missing.example") return [];
      return [
        { tag: "ipv6", val: [0, 0, 0, 0, 0, 0xffff, 0x0a00, 7] },
        { tag: "ipv4", val: [10, 0, 0, 8] },
      ];
    },
  };
  return { provider, calls, live, lookups, received };
}

async function artifact(): Promise<Uint8Array> {
  const response = await fetch("/compiled-sockets.wasm");
  if (!response.ok) throw new Error("Missing compiled sockets fixture");
  return new Uint8Array(await response.arrayBuffer());
}

const grants = {
  network: [
    {
      provider: "test",
      connect: [7401, 7402, 7403, 7407, 7409, 7499].map((port) => `127.0.0.1:${port}`),
      listen: ["127.0.0.1:7400", "127.0.0.1:7404", "127.0.0.1:7408"],
      bind: ["127.0.0.1:0"],
      datagrams: ["127.0.0.1:7405", "127.0.0.1:7406"],
      resolve: true,
    },
  ],
};

async function using(
  base: object,
  components: PreparedComponent[],
  run: (host: Host, probe: Reference, net: ReturnType<typeof network>) => Promise<void>,
): Promise<void> {
  const net = network();
  const host = await createHost({
    isolated: { ...base, components, providers: { network: { test: net.provider } } },
  });
  let probe: Reference | undefined;
  try {
    await host.install({ artifact: await artifact(), grants });
    probe = (await host.get("compiled-sockets::example:sockets/probe@1.0.0", "main")) as Reference;
    await run(host, probe, net);
  } finally {
    probe?.release();
    await host.close();
    same(net.live.size, 0, "no physical socket survives close");
    const before = JSON.stringify(net.calls);
    await tick();
    same(JSON.stringify(net.calls), before, "no provider work after close");
    for (const [key, value] of Object.entries(host.diagnostics()))
      if (
        [
          "objects",
          "references",
          "roots",
          "calls",
          "resources",
          "tasks",
          "orbs",
          "values",
          "pendingJobs",
        ].includes(key)
      )
        same(value, 0, `${key} after socket close`);
  }
}

/** Exact generated WASI imports for TCP, UDP, DNS, initializer errors and send completion. */
export async function verifyCompiledSocketOperations(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, probe, net) => {
    const initialized = ok(await probe.initialized(), "initializer receive");
    if (!initialized.startsWith("read=0 status=end completion=Err("))
      throw new Error(`Unexpected initializer state: ${initialized}`);
    same(ok(await probe["read-drop"](7401), "read-drop"), "hello", "compiled early read drop");
    same(net.calls["tcp.read.cancel"], 1, "early reader physically cancelled");
    same(ok(await probe["udp-echo"](7405, "ping"), "UDP echo"), "echo:ping", "compiled datagrams");
    same(
      ok(await probe.serve(7400, 3), "serve"),
      ["client-0", "client-1", "client-2"],
      "accepted resource stream",
    );
    same(net.calls["tcp.accept"], 3, "one provider accept per consumed socket");
    same(ok(await probe.resolve("127.0.0.1"), "literal"), ["127.0.0.1"], "literal resolver");
    same(net.calls["dns.resolve"] ?? 0, 0, "literal never reaches provider");
    same(
      ok(await probe.resolve("bücher.example"), "IDNA"),
      ["10.0.0.7", "10.0.0.8"],
      "mapped IPv6 canonicalization",
    );
    same(net.lookups, ["xn--bcher-kva.example"], "ASCII provider hostname");
    error(await probe.resolve("missing.example"), "NameUnresolvable", "empty lookup");
    error(await probe.resolve("busy.example"), "TemporaryResolverFailure", "typed lookup error");
    const before = net.calls["dns.resolve"];
    error(await probe.resolve("bad name"), "InvalidArgument", "invalid lookup");
    same(net.calls["dns.resolve"], before, "invalid name refused before provider");
    error(await probe.connect(7409), "ConnectionRefused", "typed TCP error");
    same(
      ok(await probe["send-blocked"](7403, 131072), "normal send"),
      "sent",
      "send future completion",
    );
    same(net.received.length, 131072, "full guest send reached provider");
    same(
      net.received.every((byte) => byte === 115),
      true,
      "guest send content",
    );
    same(net.calls["tcp.write.close"], 1, "send performs physical write shutdown");
    same(host.errors().length, 0, "compiled socket operations report no hidden failures");
  });
}

/** Permission failures precede network effects, and a missing provider is not emulated. */
export async function verifyCompiledSocketRefusals(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, probe, net) => {
    const connects = net.calls["tcp.connect"] ?? 0;
    const sends = net.calls["udp.send"] ?? 0;
    const binds = net.calls["tcp.bind"] ?? 0;
    error(await probe.connect(7410), "AccessDenied", "denied connect");
    error(await probe["udp-echo"](7410, "no"), "AccessDenied", "denied datagram");
    error(await probe.serve(7410, 1), "AccessDenied", "denied listener");
    same(
      [net.calls["tcp.connect"] ?? 0, net.calls["udp.send"] ?? 0, net.calls["tcp.bind"] ?? 0],
      [connects, sends, binds],
      "denied operations have no provider effects",
    );
    same(host.errors().length, 0, "permission refusal is not quarantine");
    await host.reload([
      {
        artifact: await artifact(),
        grants: { network: [{ provider: "test", connect: ["127.0.0.1:7401"] }] },
      },
    ]);
    const lookups = net.calls["dns.resolve"] ?? 0;
    error(await probe.resolve("service.example"), "AccessDenied", "denied DNS");
    error(await probe.resolve("127.0.0.1"), "AccessDenied", "denied literal lookup");
    error(
      await probe.resolve("bad name"),
      "AccessDenied",
      "grant check before DNS argument parsing",
    );
    same(net.calls["dns.resolve"] ?? 0, lookups, "denied DNS has no provider effect");
  });
  const host = await createHost({
    isolated: { ...base, components, providers: { network: { test: {} } } },
  });
  let probe: Reference | undefined;
  try {
    await host.install({ artifact: await artifact(), grants });
    probe = (await host.get("compiled-sockets::example:sockets/probe@1.0.0", "main")) as Reference;
    error(await probe.connect(7401), "NotSupported", "absent TCP capability");
    error(await probe["udp-echo"](7405, "no"), "NotSupported", "absent UDP capability");
    error(
      await probe.resolve("service.example"),
      "PermanentResolverFailure",
      "absent DNS capability",
    );
    same(host.errors().length, 0, "unavailable capability is a standard result");
  } finally {
    probe?.release();
    await host.close();
  }
}

/** Retained handles survive callbacks, explicit drops and reload release their physical backends. */
export async function verifyCompiledSocketRetention(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, probe, net) => {
    ok(await probe["retain-receive"](7402), "retain receive");
    same(ok(await probe["read-retained"](100), "read retained"), "abc", "retained receive EOF");
    same(
      ok(await probe["retained-completion"](false), "retained EOF completion"),
      "ok",
      "clean EOF result",
    );
    ok(await probe["retain-receive"](7403), "retain silent receive");
    same(
      ok(await probe["retained-completion"](true), "explicit stream drop"),
      "ok",
      "explicit DROP result",
    );
    ok(await probe["retain-receive"](7401), "retain partial receive");
    await host.root(async (context) => {
      const before = net.calls["tcp.read"] ?? 0;
      const pending = probe["read-retained"](100, context).catch((failure: unknown) => failure);
      await until(() => (net.calls["tcp.read"] ?? 0) > before, "retained read after its prefix");
      context.cancel();
      if (!((await pending) instanceof Error)) throw new Error("Cancelled retained read succeeded");
    });
    const cancelled = ok(await probe["retained-completion"](false), "cancelled read completion");
    if (!cancelled.includes("Operation cancelled"))
      throw new Error(`Cancelled receive completion became clean EOF: ${cancelled}`);
    ok(await probe["retain-receive"](7403), "retain for explicit cleanup");
    ok(await probe["retain-listen"](7404), "retain listener");
    await probe["drop-retained"]();
    same(net.live.size, 1, "only initializer socket remains after guest drop");
    ok(await probe["retain-receive"](7403), "retain before reload");
    ok(await probe["retain-listen"](7404), "retain listener before reload");
    await host.reload([{ artifact: await artifact(), grants }]);
    await until(() => net.live.size === 1, "old revision socket retirement after reload");
    same(net.live.size, 1, "reload leaves only replacement initializer socket");
    same(
      ok(await probe.resolve("127.0.0.1"), "replacement resolves"),
      ["127.0.0.1"],
      "replacement serves",
    );
    same(host.errors().length, 0, "retention and retirement have no hidden failures");
  });
}

/** Pending provider operations settle after caller cancellation without quarantining the Orb. */
export async function verifyCompiledSocketCancellation(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  for (const [method, args, observed, stoppedOperation] of [
    ["connect", [7499], "tcp.connect", "tcp.connect.aborted"],
    ["read-pending", [7403], "tcp.read", "tcp.read.cancel"],
    ["accept-pending", [7404], "tcp.accept", "tcp.accept.aborted"],
    ["accept-pending", [7408], "tcp.accept", "tcp.accept.late"],
    ["udp-echo", [7406, "pending"], "udp.receive", "udp.receive.aborted"],
    ["resolve", ["pending.example"], "dns.resolve", "dns.resolve.aborted"],
    ["send-blocked", [7407, 131072], "tcp.write", "tcp.write.aborted"],
  ] as const)
    await using(base, components, async (host, probe, net) => {
      await host.root(async (context) => {
        const before = net.calls[observed] ?? 0;
        const pending = probe[method](...args, context).catch((failure: unknown) => failure);
        await until(() => (net.calls[observed] ?? 0) > before, observed);
        context.cancel();
        const result = await pending;
        if (!(result instanceof Error)) throw new Error(`Cancelled ${method} succeeded`);
      });
      same(
        ok(await probe.resolve("127.0.0.1"), `${method} health`),
        ["127.0.0.1"],
        `${method} keeps serving`,
      );
      same(net.calls[stoppedOperation], 1, `${method} physically stopped`);
      same(net.live.size, 1, `${method} leaves only initializer backend, including late accepts`);
      same(host.errors().length, 0, `${method} cancellation is cooperative`);
    });
}

/** Every pinned TCP/UDP option getter/setter, address/state method and peer association. */
export async function verifyCompiledSocketOptions(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, probe, net) => {
    const options = ok(await probe["socket-options"](), "socket option roundtrips");
    same(
      options,
      [
        "tcp.get_keep_alive_enabled",
        "tcp.get_keep_alive_idle_time",
        "tcp.get_keep_alive_interval",
        "tcp.get_keep_alive_count",
        "tcp.get_hop_limit",
        "tcp.get_receive_buffer_size",
        "tcp.get_send_buffer_size",
        "udp.get_unicast_hop_limit",
        "udp.get_receive_buffer_size",
        "udp.get_send_buffer_size",
        "tcp.set_keep_alive_idle_time.zero",
        "tcp.set_keep_alive_interval.zero",
        "tcp.set_keep_alive_count.zero",
        "tcp.set_hop_limit.zero",
        "tcp.set_receive_buffer_size.zero",
        "tcp.set_send_buffer_size.zero",
        "tcp.set_listen_backlog_size.zero",
        "udp.set_unicast_hop_limit.zero",
        "udp.set_receive_buffer_size.zero",
        "udp.set_send_buffer_size.zero",
      ],
      "all option getters, setters and zero refusals executed through compiled WIT",
    );
    for (const [key, value] of Object.entries(net.calls))
      if (
        key.startsWith("tcp.get.") ||
        key.startsWith("tcp.set.") ||
        key.startsWith("udp.get.") ||
        key.startsWith("udp.set.")
      )
        same(value, 1, `${key}: invalid zero setter never reaches provider`);
    same(
      ok(await probe["socket-addresses"](7403, 7404), "TCP address operations"),
      ["tcp.addresses", "tcp.listening", "tcp.backlog", "tcp.families"],
      "TCP addresses, families and state",
    );
    same(net.calls["tcp.backlog"], 0x7fffffff, "backlog clamps before listen");
    same(net.calls["tcp.set.listenBacklogSize"], 1, "listening backlog reaches provider");
    same(
      ok(await probe["udp-connected"](7405), "UDP association operations"),
      [
        "udp.bind",
        "udp.addresses",
        "udp.connect",
        "udp.connected-io",
        "udp.disconnect",
        "udp.families",
      ],
      "UDP addresses, families and association",
    );
    same(net.calls["udp.send"], 1, "connected peer mismatch and missing peer never reach network");
    same(host.errors().length, 0, "option and address operations leave no hidden error");
  });
}
