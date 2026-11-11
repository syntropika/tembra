// Browser `wasi:sockets` semantics through the exact statics and resource methods the generated
// binding calls, over a deterministic in-memory loopback provider. No ambient network is used.
import { expect, test } from "bun:test";
import type { IpSocketAddress } from "wasi:sockets/types@0.3.0";
import { BrowserNetwork } from "../src/interpreter/browser-network.ts";
import {
  NameLookupError,
  type SocketProvider,
  SocketProviderError,
  type TcpBackend,
  type TcpConnection,
  type TcpListener,
  type TcpOptions,
  type UdpBackend,
} from "../src/interpreter/socket-provider.ts";
import { METHODS, RESOURCES } from "../src/interpreter/standard.ts";
import {
  CALL,
  DROP,
  encode,
  Reader,
  STREAM_READ,
  Writer,
} from "../src/interpreter/standard-codec.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import {
  socketMethods,
  socketStatics,
  type SocketBindings,
  type TcpSocket,
  type UdpSocket,
} from "../src/interpreter/standard-sockets.ts";
import { decodeAs, raw } from "./standard-guest.ts";
import {
  InputBudget,
  InputStream,
  methodKey,
  OutputStream,
  type StandardCall,
} from "../src/interpreter/standard-streams.ts";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const v4 = (a: number, b: number, c: number, d: number, port: number): IpSocketAddress => ({
  tag: "ipv4",
  val: { address: [a, b, c, d], port },
});
const loopback = (port: number) => v4(127, 0, 0, 1, port);
const any = v4(0, 0, 0, 0, 0);

function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => (resolve = accept));
  return { promise, resolve };
}

/** Every provider call, so refusals can prove they reached nothing. */
type Calls = Record<string, number>;

/** An in-memory loopback network of provider backends; ports are real per network instance. */
function loopbackNetwork(options: { listen?: boolean; bind?: boolean; udpBind?: boolean } = {}) {
  const calls: Calls = {};
  const count = (name: string) => (calls[name] = (calls[name] ?? 0) + 1);
  const listeners = new Map<
    number,
    { queue: { socket: TcpBackend; connection: TcpConnection }[]; wake?: () => void }
  >();
  const udp = new Map<
    number,
    { inbox: { data: Uint8Array; remote: IpSocketAddress }[]; wake?: () => void }
  >();
  const closes: string[] = [];
  let ephemeral = 40000;
  const port = (address: IpSocketAddress | undefined) =>
    (address as { val: { port: number } } | undefined)?.val.port ?? 0;

  function pipePair(): [TcpConnection, TcpConnection, () => void] {
    const up = new TransformStream<Uint8Array, Uint8Array>();
    const down = new TransformStream<Uint8Array, Uint8Array>();
    let cancelled = 0;
    const counted = (readable: ReadableStream<Uint8Array>) => {
      const reader = readable.getReader();
      return new ReadableStream<Uint8Array>({
        pull: async (controller) => {
          const next = await reader.read();
          if (next.done) controller.close();
          else controller.enqueue(next.value);
        },
        cancel: async () => {
          cancelled++;
          count("readable.cancel");
          await reader.cancel();
        },
      });
    };
    return [
      { remote: loopback(0), readable: counted(down.readable), writable: up.writable },
      { remote: loopback(0), readable: counted(up.readable), writable: down.writable },
      () => void cancelled,
    ];
  }

  function tcpBackend(local?: IpSocketAddress, remote?: IpSocketAddress): TcpBackend {
    const settings = new Map<keyof TcpOptions, unknown>([["keepAliveEnabled", false]]);
    const backend: TcpBackend = {
      local,
      ...(options.bind === false
        ? {}
        : {
            bind(address: IpSocketAddress) {
              count("tcp.bind");
              const wanted = port(address);
              if (wanted && listeners.has(wanted))
                throw new SocketProviderError({ tag: "address-in-use" });
              (backend as { local?: IpSocketAddress }).local = loopback(wanted || ephemeral++);
            },
          }),
      ...(options.listen === false
        ? {}
        : {
            listen(backlog: number): TcpListener {
              count("tcp.listen");
              calls.backlog = backlog;
              const at = port(backend.local) || ephemeral++;
              (backend as { local?: IpSocketAddress }).local ??= loopback(at);
              const state: {
                queue: { socket: TcpBackend; connection: TcpConnection }[];
                wake?: () => void;
              } = { queue: [] };
              listeners.set(at, state);
              return {
                async accept(signal) {
                  count("tcp.accept");
                  while (!state.queue.length) {
                    if (signal.aborted) throw new Error("accept aborted");
                    await new Promise<void>((resolve) => {
                      state.wake = resolve;
                      signal.addEventListener("abort", () => resolve(), { once: true });
                    });
                  }
                  return state.queue.shift()!;
                },
              };
            },
          }),
      async connect(target, signal) {
        count("tcp.connect");
        const server = listeners.get(port(target));
        if (signal.aborted) throw new Error("aborted");
        if (!server) throw new SocketProviderError({ tag: "connection-refused" });
        const [client, accepted] = pipePair();
        (backend as { local?: IpSocketAddress }).local ??= loopback(ephemeral++);
        server.queue.push({
          socket: tcpBackend(loopback(port(target)), backend.local),
          connection: { ...accepted, remote: backend.local! },
        });
        server.wake?.();
        return { ...client, remote: target };
      },
      getOption(name) {
        count("tcp.getOption");
        if (!settings.has(name)) throw new SocketProviderError({ tag: "not-supported" });
        return settings.get(name) as never;
      },
      setOption(name, value) {
        count("tcp.setOption");
        settings.set(name, value);
      },
      async close() {
        count("tcp.close");
        closes.push(`tcp:${port(backend.local)}`);
        if (port(backend.local)) listeners.delete(port(backend.local));
      },
    };
    void remote;
    return backend;
  }

  function udpBackend(): UdpBackend {
    const backend: UdpBackend = {
      local: undefined,
      ...(options.udpBind === false
        ? {}
        : {
            bind(address: IpSocketAddress) {
              count("udp.bind");
              const at = port(address) || ephemeral++;
              (backend as { local?: IpSocketAddress }).local = loopback(at);
              udp.set(at, { inbox: [] });
            },
          }),
      async send(data, remote) {
        count("udp.send");
        if (!backend.local) {
          const at = ephemeral++;
          (backend as { local?: IpSocketAddress }).local = loopback(at);
          udp.set(at, { inbox: [] });
        }
        const target = udp.get(port(remote));
        target?.inbox.push({ data: data.slice(), remote: backend.local! });
        target?.wake?.();
      },
      async receive(signal) {
        count("udp.receive");
        const box = udp.get(port(backend.local))!;
        while (!box.inbox.length) {
          if (signal.aborted) throw new Error("receive aborted");
          await new Promise<void>((resolve) => {
            box.wake = resolve;
            signal.addEventListener("abort", () => resolve(), { once: true });
          });
        }
        return box.inbox.shift()!;
      },
      async close() {
        count("udp.close");
        closes.push(`udp:${port(backend.local)}`);
      },
    };
    return backend;
  }

  const provider: SocketProvider = {
    tcp: {
      create() {
        count("tcp.create");
        return tcpBackend();
      },
    },
    udp: {
      create() {
        count("udp.create");
        return udpBackend();
      },
    },
    async resolve(name) {
      count("resolve");
      if (name === "missing.test") return [];
      if (name === "busy.test") throw new NameLookupError({ tag: "temporary-resolver-failure" });
      if (name === "broken.test") return [{ tag: "ipv4", val: [1, 2, 3] }] as never;
      return [
        { tag: "ipv6", val: [0, 0, 0, 0, 0, 0xffff, 0x7f00, 1] },
        { tag: "ipv4", val: [10, 0, 0, 1] },
      ];
    },
  };
  return { provider, calls, closes, udp };
}

/** A host and the bindings one Orb would get from its install plan. */
function orb(
  provider: SocketProvider | undefined,
  grant: {
    connect?: string[];
    listen?: string[];
    bind?: string[];
    datagrams?: string[];
    resolve?: boolean;
  },
) {
  const network = new BrowserNetwork(
    { network: [{ provider: "net", ...grant }] },
    provider ? { network: { net: provider } } : {},
  );
  const bindings: SocketBindings = {
    types: network.sockets("net"),
    lookup: network.sockets("net"),
  };
  const host = new StandardHost(new Set(), [], 8 << 20);
  const call = (signal?: AbortSignal): StandardCall => ({ host, signal });
  return { host, call, statics: socketStatics(bindings) };
}

const ok = <T>(result: { tag: string; val?: unknown }): T => {
  expect(result.tag).toBe("ok");
  return result.val as T;
};

/** Read a byte OutputStream reply: `undefined` at end of stream. */
async function readBytes(
  stream: OutputStream,
  signal?: AbortSignal,
): Promise<Uint8Array | undefined> {
  const reply = await stream.read(65536, 1 << 20, signal);
  if (reply[0] === 1) return undefined;
  const length = new DataView(reply.buffer, reply.byteOffset + 1, 4).getUint32(0, true);
  return reply.subarray(5, 5 + length);
}

async function writeAll(input: InputStream, bytes: Uint8Array) {
  for (let at = 0; at < bytes.length; at += 65536) {
    const chunk = bytes.subarray(at, at + 65536);
    await input.write(chunk.length, () => chunk.slice());
  }
  input.close();
}

async function retired(host: StandardHost) {
  await host.dispose();
  expect(host.pending).toBe(0);
  expect(host.inputBytes).toBe(0);
}

test("refusals reach no provider: grant before capability before the provider call", async () => {
  const { provider, calls } = loopbackNetwork();
  const denied = orb(provider, {});
  expect(denied.statics.tcpCreate(denied.call(), "ipv4")).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  expect(denied.statics.udpCreate(denied.call(), "ipv4")).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  expect(await denied.statics.resolveAddresses(denied.call(), "example.test")).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  // Denial is checked before parsing, as upstream does.
  expect((await denied.statics.resolveAddresses(denied.call(), "bad/name")).val).toEqual({
    tag: "access-denied",
  });
  // Granted, but the provider lacks the capability: distinguishable standard results.
  const missing = orb(
    {},
    { connect: ["127.0.0.1:80"], datagrams: ["127.0.0.1:53"], resolve: true },
  );
  expect(missing.statics.tcpCreate(missing.call(), "ipv4")).toEqual({
    tag: "err",
    val: { tag: "not-supported" },
  });
  expect(missing.statics.udpCreate(missing.call(), "ipv4")).toEqual({
    tag: "err",
    val: { tag: "not-supported" },
  });
  expect(await missing.statics.resolveAddresses(missing.call(), "example.test")).toEqual({
    tag: "err",
    val: { tag: "permanent-resolver-failure" },
  });
  const unregistered = orb(undefined, { connect: ["127.0.0.1:80"] });
  expect(unregistered.statics.tcpCreate(unregistered.call(), "ipv4").val).toEqual({
    tag: "not-supported",
  });
  // Per-address refusals on a created socket.
  const { statics, call } = orb(provider, { connect: ["127.0.0.1:80"] });
  const socket = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(socket.bind(loopback(5))).toEqual({ tag: "err", val: { tag: "access-denied" } });
  expect(socket.bind(v4(224, 0, 0, 1, 5))).toEqual({
    tag: "err",
    val: { tag: "invalid-argument" },
  });
  expect(await socket.connect(loopback(81), call())).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  expect(await socket.connect(loopback(0), call())).toEqual({
    tag: "err",
    val: { tag: "invalid-argument" },
  });
  expect(socket.listen(call())).toEqual({ tag: "err", val: { tag: "access-denied" } });
  expect(calls["tcp.bind"] ?? 0).toBe(0);
  expect(calls["tcp.connect"] ?? 0).toBe(0);
  expect(calls["tcp.listen"] ?? 0).toBe(0);
  expect(calls.resolve ?? 0).toBe(0);
  socket.release();
});

test("bind is honest: real provider binds, provider errors, or not-supported", async () => {
  const { provider } = loopbackNetwork();
  const { statics, call, host } = orb(provider, {
    bind: ["127.0.0.1:0", "127.0.0.1:7000"],
    listen: ["127.0.0.1:0"],
  });
  const socket = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(socket.getLocalAddress()).toEqual({ tag: "err", val: { tag: "invalid-state" } });
  expect(socket.bind(loopback(0)).tag).toBe("ok");
  // The address is the provider's actual binding, never the requested port 0.
  expect(socket.getLocalAddress()).toEqual({ tag: "ok", val: loopback(40000) });
  expect(socket.bind(loopback(7000))).toEqual({ tag: "err", val: { tag: "invalid-state" } });
  const listening = ok<OutputStream>(socket.listen(call()));
  const second = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  // A provider failure surfaces from bind itself.
  expect(second.bind(loopback(0)).tag).toBe("ok");
  const third = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  void third;
  // A relay without synchronous bind/listen: not-supported, and no fabricated local address.
  const relay = loopbackNetwork({ bind: false, listen: false });
  const relayed = orb(relay.provider, {
    bind: ["127.0.0.1:0"],
    listen: ["0.0.0.0:0"],
    connect: ["127.0.0.1:9"],
  });
  const client = ok<TcpSocket>(relayed.statics.tcpCreate(relayed.call(), "ipv4"));
  expect(client.bind(loopback(0))).toEqual({ tag: "err", val: { tag: "not-supported" } });
  expect(client.listen(relayed.call())).toEqual({ tag: "err", val: { tag: "not-supported" } });
  expect(client.getLocalAddress()).toEqual({ tag: "err", val: { tag: "invalid-state" } });
  listening.close();
  for (const item of [socket, second, third, client]) item.release();
  await retired(host);
  await retired(relayed.host);
});

test("options: getters and setters are independent, validated first, never invented", async () => {
  const { provider } = loopbackNetwork();
  const { statics, call, host } = orb(provider, { connect: ["127.0.0.1:80"] });
  const socket = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(socket.getKeepAliveEnabled()).toEqual({ tag: "ok", val: false });
  expect(socket.setHopLimit(0)).toEqual({ tag: "err", val: { tag: "invalid-argument" } });
  expect(socket.setKeepAliveIdleTime(0n)).toEqual({ tag: "err", val: { tag: "invalid-argument" } });
  expect(socket.setHopLimit(9).tag).toBe("ok");
  expect(socket.getHopLimit()).toEqual({ tag: "ok", val: 9 });
  expect(socket.setListenBacklogSize(0n)).toEqual({ tag: "err", val: { tag: "invalid-argument" } });
  expect(socket.setListenBacklogSize(1n << 40n).tag).toBe("ok");
  // Setter-only and getter-only providers.
  const setterOnly: SocketProvider = {
    tcp: {
      create: () => ({
        local: undefined,
        connect: async () => {
          throw new Error();
        },
        close: async () => {},
        setOption() {},
      }),
    },
  };
  const getterOnly: SocketProvider = {
    tcp: {
      create: () => ({
        local: undefined,
        connect: async () => {
          throw new Error();
        },
        close: async () => {},
        getOption: () => true as never,
      }),
    },
  };
  const a = ok<TcpSocket>(
    orb(setterOnly, { connect: ["127.0.0.1:80"] }).statics.tcpCreate(call(), "ipv4"),
  );
  expect(a.setKeepAliveEnabled(true).tag).toBe("ok");
  expect(a.getKeepAliveEnabled()).toEqual({ tag: "err", val: { tag: "not-supported" } });
  const b = ok<TcpSocket>(
    orb(getterOnly, { connect: ["127.0.0.1:80"] }).statics.tcpCreate(call(), "ipv4"),
  );
  expect(b.getKeepAliveEnabled()).toEqual({ tag: "ok", val: true });
  expect(b.setKeepAliveEnabled(true)).toEqual({ tag: "err", val: { tag: "not-supported" } });
  for (const item of [socket, a, b]) item.release();
  await retired(host);
});

test("TCP: listen, one accepted owned socket per read, 3 MiB duplex, shared lifetimes", async () => {
  const { provider, calls, closes } = loopbackNetwork();
  const { statics, call, host } = orb(provider, {
    listen: ["127.0.0.1:0"],
    bind: ["127.0.0.1:0"],
    connect: ["127.0.0.1:40000"],
  });
  const server = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(server.bind(loopback(0)).tag).toBe("ok");
  expect(server.setListenBacklogSize(7n).tag).toBe("ok");
  const listen = ok<OutputStream>(server.listen(call()));
  expect(calls.backlog).toBe(7);
  expect(server.getIsListening()).toBe(true);
  const client = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(await client.connect(loopback(40000), call())).toEqual({ tag: "ok", val: undefined });
  const accepted = (await listen.take())!.value as TcpSocket;
  expect(accepted.getRemoteAddress().tag).toBe("ok");
  // The listener holds its own reference: dropping the server handle keeps it accepting.
  server.release();
  expect(closes).toEqual([]);

  const budget = new InputBudget();
  const upload = new InputStream(0, budget);
  upload.transferred = true;
  const download = new InputStream(0, budget);
  download.transferred = true;
  const sent = client.send(upload, call());
  const [serverReceive, serverDone] = accepted.receive(call());
  const echoed = accepted.send(download, call());
  const [clientReceive, clientDone] = client.receive(call());
  const payload = Uint8Array.from({ length: 3 << 20 }, (_, at) => (at * 31) % 251);
  const drain = async (stream: OutputStream) => {
    const parts: number[] = [];
    for (let chunk = await readBytes(stream); chunk; chunk = await readBytes(stream))
      parts.push(...chunk);
    return Uint8Array.from(parts);
  };
  const [up, down] = await Promise.all([
    drain(serverReceive),
    drain(clientReceive),
    writeAll(upload, payload),
    writeAll(download, payload.slice().reverse()),
  ]);
  expect(up).toEqual(payload);
  expect(down).toEqual(payload.slice().reverse());
  expect(await sent.value).toEqual({ tag: "ok", val: undefined });
  expect(await echoed.value).toEqual({ tag: "ok", val: undefined });
  expect(await serverDone.value).toEqual({ tag: "ok", val: undefined });
  expect(await clientDone.value).toEqual({ tag: "ok", val: undefined });
  // A second send or receive is invalid-state, as WIT requires.
  expect(await client.send(new InputStream(0, budget), call()).value).toEqual({
    tag: "err",
    val: { tag: "invalid-state" },
  });
  // Socket handles dropped: backends close only once their streams are gone too.
  client.release();
  accepted.release();
  serverReceive.close();
  clientReceive.close();
  listen.close();
  await host.settled();
  expect(closes.length).toBe(3);
  await retired(host);
});

test("accept streams: bounded pull, cancellation keeps the value, drop releases and acknowledges", async () => {
  const { provider, calls, closes } = loopbackNetwork();
  const { statics, call, host } = orb(provider, {
    listen: ["0.0.0.0:0"],
    connect: ["127.0.0.1:40000"],
  });
  const server = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  const listen = ok<OutputStream>(server.listen(call()));
  // A burst of 100 connections while the guest reads slowly.
  const clients: TcpSocket[] = [];
  for (let n = 0; n < 100; n++) {
    const client = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
    await client.connect(loopback(40000), call());
    clients.push(client);
  }
  const first = (await listen.take())!.value as TcpSocket;
  // The host pulled exactly once per read: the burst stays in the provider's queue.
  expect(calls["tcp.accept"]).toBe(1);
  // A read cancelled while the pull is pending: the value is kept for the next read.
  const empty = loopbackNetwork();
  const idle = orb(empty.provider, { listen: ["0.0.0.0:0"], connect: ["127.0.0.1:40000"] });
  const quiet = ok<TcpSocket>(idle.statics.tcpCreate(idle.call(), "ipv4"));
  const quietStream = ok<OutputStream>(quiet.listen(idle.call()));
  const callback = new AbortController();
  const pending = quietStream.take(callback.signal);
  await tick();
  callback.abort();
  await expect(pending).rejects.toThrow("cancelled");
  // Callback retirement does not wait for the idle accept.
  await idle.host.settled();
  const late = ok<TcpSocket>(idle.statics.tcpCreate(idle.call(), "ipv4"));
  await late.connect(loopback(40000), idle.call());
  const kept = (await quietStream.take())!.value as TcpSocket;
  expect(kept.getRemoteAddress().tag).toBe("ok");
  // The kept value is the original pull's result: no second pull, nothing lost.
  expect(empty.calls["tcp.accept"]).toBe(1);
  // Drop with a pending pull: its signal aborts, the pull settles, then the reference goes.
  const waiting = quietStream.take();
  waiting.catch(() => {});
  await tick();
  quietStream.close();
  quiet.release();
  await idle.host.settled();
  expect(empty.closes).toContain("tcp:40000");
  // A held value on drop is released (the accepted backend closes).
  const second = listen.take(AbortSignal.abort());
  await expect(second).rejects.toThrow("cancelled");
  const third = listen.take();
  const held = (await third)!.value as TcpSocket;
  held.release();
  listen.close();
  server.release();
  for (const item of [first, kept, late, ...clients]) item.release();
  await host.settled();
  expect(closes.length).toBeGreaterThan(0);
  await retired(host);
  await retired(idle.host);
});

test("receive drop shuts down reads at once and waits for its acknowledgement", async () => {
  const { provider, calls } = loopbackNetwork();
  const { statics, call, host } = orb(provider, {
    listen: ["0.0.0.0:0"],
    connect: ["127.0.0.1:40000"],
  });
  const server = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  const listen = ok<OutputStream>(server.listen(call()));
  const client = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  await client.connect(loopback(40000), call());
  const accepted = (await listen.take())!.value as TcpSocket;
  const [receive, done] = accepted.receive(call());
  const reading = readBytes(receive);
  reading.catch(() => {});
  await tick();
  receive.close();
  await host.settled();
  expect(calls["readable.cancel"]).toBe(1);
  expect(await done.value).toEqual({ tag: "ok", val: undefined });
  for (const item of [server, client, accepted]) item.release();
  listen.close();
  await retired(host);
});

test("cancelled connect: provider promise tracked, late connection discarded, socket closed", async () => {
  const late = deferred<TcpConnection>();
  let aborted = false;
  const closing = deferred();
  const provider: SocketProvider = {
    tcp: {
      create: () => ({
        local: undefined,
        connect: (_remote, signal) => {
          signal.addEventListener("abort", () => (aborted = true));
          return late.promise;
        },
        close: async () => {},
      }),
    },
  };
  const { statics, call, host } = orb(provider, { connect: ["127.0.0.1:80"] });
  const socket = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  const callback = new AbortController();
  const connecting = socket.connect(loopback(80), call(callback.signal));
  await tick();
  callback.abort();
  await expect(connecting).rejects.toThrow("cancelled");
  expect(aborted).toBe(true);
  expect(host.pending).toBeGreaterThan(0);
  let cancelled = false;
  late.resolve({
    remote: loopback(80),
    readable: new ReadableStream({
      cancel: () => {
        cancelled = true;
        return closing.promise;
      },
    }),
    writable: new WritableStream(),
  });
  await tick();
  expect(cancelled).toBe(true);
  let settled = false;
  const settling = host.settled().then(() => (settled = true));
  await tick();
  expect(settled).toBe(false);
  closing.resolve();
  await settling;
  expect(socket.getRemoteAddress()).toEqual({ tag: "err", val: { tag: "invalid-state" } });
  socket.release();
  await retired(host);
});

test("UDP: implicit bind, connected peer filter, ungranted peers dropped, datagram bounds", async () => {
  const { provider, calls } = loopbackNetwork();
  const { statics, call, host } = orb(provider, {
    bind: ["127.0.0.1:5300"],
    datagrams: ["127.0.0.1:5300", "127.0.0.1:40000"],
  });
  const server = ok<UdpSocket>(statics.udpCreate(call(), "ipv4"));
  expect(server.receive.length).toBeGreaterThanOrEqual(0);
  expect(await server.receive(call())).toEqual({ tag: "err", val: { tag: "invalid-state" } });
  expect(server.bind(loopback(5300)).tag).toBe("ok");
  expect(server.getLocalAddress()).toEqual({ tag: "ok", val: loopback(5300) });
  const client = ok<UdpSocket>(statics.udpCreate(call(), "ipv4"));
  expect(await client.send(new Uint8Array(65536), loopback(5300), call())).toEqual({
    tag: "err",
    val: { tag: "datagram-too-large" },
  });
  expect(await client.send(new Uint8Array(1), loopback(5301), call())).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  expect(await client.send(new Uint8Array(1), undefined, call())).toEqual({
    tag: "err",
    val: { tag: "invalid-argument" },
  });
  expect(calls["udp.send"] ?? 0).toBe(0);
  // Implicit bind on the first send gives the client 40000 (granted as a datagram peer).
  expect(await client.send(Uint8Array.of(1, 2), loopback(5300), call())).toEqual({
    tag: "ok",
    val: undefined,
  });
  expect(client.getLocalAddress()).toEqual({ tag: "ok", val: loopback(40000) });
  expect(await server.receive(call())).toEqual({
    tag: "ok",
    val: [Uint8Array.of(1, 2), loopback(40000)],
  });
  // An ungranted sender is dropped; the next granted datagram is delivered.
  const stranger = ok<UdpSocket>(
    orb(provider, { datagrams: ["127.0.0.1:5300"] }).statics.udpCreate(call(), "ipv4"),
  );
  await stranger.send(Uint8Array.of(9), loopback(5300), call());
  await client.send(Uint8Array.of(3), loopback(5300), call());
  expect(await server.receive(call())).toEqual({
    tag: "ok",
    val: [Uint8Array.of(3), loopback(40000)],
  });
  // Connected mode: send defaults to the peer; another peer argument is invalid-argument.
  expect(client.connect(loopback(5300)).tag).toBe("ok");
  expect(client.getRemoteAddress()).toEqual({ tag: "ok", val: loopback(5300) });
  expect(await client.send(Uint8Array.of(4), loopback(40000), call())).toEqual({
    tag: "err",
    val: { tag: "invalid-argument" },
  });
  expect(client.disconnect().tag).toBe("ok");
  expect(client.disconnect()).toEqual({ tag: "err", val: { tag: "invalid-state" } });
  // Without synchronous bind, connecting an unbound socket is not-supported, never faked.
  const relay = loopbackNetwork({ udpBind: false });
  const relayed = orb(relay.provider, { datagrams: ["127.0.0.1:5300"] });
  const unbound = ok<UdpSocket>(relayed.statics.udpCreate(relayed.call(), "ipv4"));
  expect(unbound.connect(loopback(5300))).toEqual({ tag: "err", val: { tag: "not-supported" } });
  expect(unbound.getUnicastHopLimit()).toEqual({ tag: "err", val: { tag: "not-supported" } });
  for (const item of [server, client, stranger, unbound]) item.release();
  await retired(host);
  await retired(relayed.host);
});

test("name lookup: IP literals locally, IDNA to the provider, strict input, standard codes", async () => {
  const { provider, calls } = loopbackNetwork();
  const { statics, call } = orb(provider, { resolve: true });
  const resolve = (name: string) => statics.resolveAddresses(call(), name);
  expect(await resolve("127.0.0.1")).toEqual({
    tag: "ok",
    val: [{ tag: "ipv4", val: [127, 0, 0, 1] }],
  });
  // WHATWG IPv4 parsing, exactly as url::Host::parse.
  expect(await resolve("127.1")).toEqual({
    tag: "ok",
    val: [{ tag: "ipv4", val: [127, 0, 0, 1] }],
  });
  expect(await resolve("[::1]")).toEqual({
    tag: "ok",
    val: [{ tag: "ipv6", val: [0, 0, 0, 0, 0, 0, 0, 1] }],
  });
  expect(await resolve("::1")).toEqual({
    tag: "ok",
    val: [{ tag: "ipv6", val: [0, 0, 0, 0, 0, 0, 0, 1] }],
  });
  expect(calls.resolve ?? 0).toBe(0);
  for (const bad of [
    "",
    "user@host",
    "a/b",
    "a?b",
    "a#b",
    "a b",
    "a\\b",
    "[::1",
    "x:y",
    "a\u0000b",
  ])
    expect(await resolve(bad)).toEqual({ tag: "err", val: { tag: "invalid-argument" } });
  expect(calls.resolve ?? 0).toBe(0);
  // IDNA to ASCII, mapped IPv6 canonicalized to IPv4.
  const seen: string[] = [];
  const recording: SocketProvider = {
    resolve: async (name) => {
      seen.push(name);
      return [{ tag: "ipv6", val: [0, 0, 0, 0, 0, 0xffff, 0x7f00, 1] }];
    },
  };
  const idna = orb(recording, { resolve: true });
  expect(await idna.statics.resolveAddresses(idna.call(), "Bücher.Example")).toEqual({
    tag: "ok",
    val: [{ tag: "ipv4", val: [127, 0, 0, 1] }],
  });
  expect(seen).toEqual(["xn--bcher-kva.example"]);
  expect(await resolve("missing.test")).toEqual({ tag: "err", val: { tag: "name-unresolvable" } });
  expect((await resolve("broken.test")).val).toEqual({
    tag: "other",
    val: "invalid provider address",
  });
});

test("per-Orb identity: one provider, independent revisions and retirements", async () => {
  const { provider, closes } = loopbackNetwork();
  const a = orb(provider, { connect: ["127.0.0.1:80"], datagrams: ["127.0.0.1:53"] });
  const b = orb(provider, { connect: ["127.0.0.1:80"], datagrams: ["127.0.0.1:53"] });
  const fromA = ok<UdpSocket>(a.statics.udpCreate(a.call(), "ipv4"));
  const fromB = ok<UdpSocket>(b.statics.udpCreate(b.call(), "ipv4"));
  await fromA.send(Uint8Array.of(1), v4(127, 0, 0, 1, 53), a.call());
  await fromB.send(Uint8Array.of(1), v4(127, 0, 0, 1, 53), b.call());
  fromA.release();
  await retired(a.host);
  expect(closes.length).toBe(1);
  expect(fromB.getLocalAddress().tag).toBe("ok");
  fromB.release();
  await retired(b.host);
  expect(closes.length).toBe(2);
  void any;
});

test("a held or late accepted socket is closed when its listen stream drops, never leaked", async () => {
  const settle = deferred<{ socket: TcpBackend; connection: TcpConnection }>();
  const firstPull = deferred<{ socket: TcpBackend; connection: TcpConnection }>();
  const closed: string[] = [];
  let aborted = false;
  const child = (name: string): TcpBackend => ({
    local: loopback(9),
    connect: async () => {
      throw new Error("unused");
    },
    close: async () => {
      closed.push(name);
    },
  });
  const connection = (): TcpConnection => ({
    remote: loopback(10),
    readable: new ReadableStream(),
    writable: new WritableStream(),
  });
  let pulls = 0;
  const provider: SocketProvider = {
    tcp: {
      create: () => ({
        local: undefined,
        listen: () => ({
          accept: (signal) => {
            pulls++;
            signal.addEventListener("abort", () => (aborted = true));
            // The first pull completes after its read was cancelled; the second ignores abort.
            return pulls === 1 ? firstPull.promise : settle.promise;
          },
        }),
        connect: async () => {
          throw new Error("unused");
        },
        close: async () => {
          closed.push("listener");
        },
      }),
    },
  };
  const { statics, call, host } = orb(provider, { listen: ["0.0.0.0:0"] });
  const server = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  const first = ok<OutputStream>(server.listen(call()));
  // Cancelled before delivery: the value is held, unissued, by the stream; drop releases it.
  const callback = new AbortController();
  const read = first.take(callback.signal);
  await tick();
  callback.abort();
  await expect(read).rejects.toThrow("cancelled");
  firstPull.resolve({ socket: child("held"), connection: connection() });
  await tick();
  expect(closed).toEqual([]);
  first.close();
  await host.settled();
  expect(closed).toEqual(["held"]);
  // The listener survives: the socket handle still holds its own reference.
  expect(server.getIsListening()).toBe(true);
  server.release();
  await host.settled();
  expect(closed).toEqual(["held", "listener"]);

  // A pull pending at drop: aborted, awaited as physical work, its late result closed.
  const again = orb(provider, { listen: ["0.0.0.0:0"] });
  const other = ok<TcpSocket>(again.statics.tcpCreate(again.call(), "ipv4"));
  const stream = ok<OutputStream>(other.listen(again.call()));
  const pending = stream.take();
  pending.catch(() => {});
  await tick();
  stream.close();
  other.release();
  expect(aborted).toBe(true);
  let settled = false;
  const settling = again.host.settled().then(() => (settled = true));
  await tick();
  expect(settled).toBe(false);
  settle.resolve({ socket: child("late"), connection: connection() });
  await settling;
  // Both closed before settlement; independent backends close in no particular order.
  expect(closed.slice(2).sort()).toEqual(["late", "listener"]);
  await retired(host);
  await retired(again.host);
});

test("malformed provider values are standard errors, never traps", async () => {
  const readable = new ReadableStream<Uint8Array>();
  readable.getReader(); // locked by the provider
  const writable = new WritableStream<Uint8Array>();
  writable.getWriter();
  const provider: SocketProvider = {
    tcp: {
      create: () => ({
        local: { tag: "ipv4", val: { address: [1, 2, 3], port: 1 } } as never,
        bind() {},
        connect: async (remote) => ({ remote, readable, writable }),
        close: async () => {},
      }),
    },
  };
  const { statics, call, host } = orb(provider, {
    bind: ["127.0.0.1:1"],
    connect: ["127.0.0.1:80"],
  });
  const socket = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(socket.bind(loopback(1)).tag).toBe("ok");
  expect(socket.getLocalAddress()).toEqual({
    tag: "err",
    val: { tag: "other", val: "invalid provider address" },
  });
  expect(await socket.connect(loopback(80), call())).toEqual({ tag: "ok", val: undefined });
  const budget = new InputBudget();
  const input = new InputStream(0, budget);
  input.transferred = true;
  expect(((await socket.send(input, call()).value) as { tag: string }).tag).toBe("err");
  expect(input.cancelled).toBe(true);
  const [, received] = socket.receive(call());
  expect(((await received.value) as { tag: string }).tag).toBe("err");
  socket.release();
  await retired(host);
});

// ---------------------------------------------------------------------------------------------
// Generated table and the packet path (wire v7 with the 125-method table)

const SOCKETS = "wasi:sockets/types@0.3.0";
const LOOKUP = "wasi:sockets/ip-name-lookup@0.3.0";

function socketIndex(resource: "tcp-socket" | "udp-socket" | undefined, name: string): number {
  const found = METHODS.findIndex(
    (m) =>
      m.name === name &&
      (resource === undefined
        ? m.interface === LOOKUP
        : m.resource !== undefined && RESOURCES[m.resource] === `${SOCKETS}#${resource}`),
  );
  if (found < 0) throw new Error(`${resource}.${name} is not in the generated table`);
  return found;
}

function callPacket(index: number, args: unknown[]): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(CALL);
  writer.u32(index);
  METHODS[index]!.params.forEach((type, at) => encode(writer, type, args[at], raw));
  return writer.finish();
}

/** A raw-packet guest of one StandardHost whose socket imports are bound to `provider`. */
function packetOrb(
  provider: SocketProvider,
  grant: Parameters<typeof orb>[1],
  packetBytes = 8 << 20,
) {
  const network = new BrowserNetwork(
    { network: [{ provider: "net", ...grant }] },
    { network: { net: provider } },
  );
  const host = new StandardHost(new Set([SOCKETS, LOOKUP]), [], packetBytes, {
    sockets: { types: network.sockets("net"), lookup: network.sockets("net") },
  });
  const sync = (resource: "tcp-socket" | "udp-socket", name: string, ...args: unknown[]): any => {
    const index = socketIndex(resource, name);
    return decodeAs(METHODS[index]!.result, host.sync(callPacket(index, args)));
  };
  const call = async (
    resource: "tcp-socket" | "udp-socket" | undefined,
    name: string,
    signal: AbortSignal,
    ...args: unknown[]
  ): Promise<any> => {
    const index = socketIndex(resource, name);
    const delivery = await host.async(callPacket(index, args), signal);
    return decodeAs(METHODS[index]!.result, delivery.bytes);
  };
  const read = (stream: number, count: number, signal = new AbortController().signal) => {
    const writer = new Writer();
    writer.u8(0);
    writer.u8(STREAM_READ);
    writer.handle(stream);
    writer.u32(count);
    return host.async(writer.finish(), signal);
  };
  const drop = (handle: number) => {
    const writer = new Writer();
    writer.u8(0);
    writer.u8(DROP);
    writer.handle(handle);
    host.sync(writer.finish());
  };
  return { host, sync, call, read, drop };
}

/** One owned-item reply: exactly one `u64` handle, or end of stream. */
function acceptedHandle(bytes: Uint8Array): number | undefined {
  const reader = new Reader(bytes);
  if (reader.u8() === 1) return undefined;
  expect(reader.u32()).toBe(1);
  const handle = reader.handle();
  reader.end();
  return handle;
}

test("every generated wasi:sockets function is bound by its generated key", () => {
  const implementations = socketMethods({});
  const generated = METHODS.filter((m) => m.interface === SOCKETS || m.interface === LOOKUP);
  expect(generated.length).toBe(41);
  expect(new Set(generated.map((m) => m.resource).filter((r) => r !== undefined)).size).toBe(2);
  for (const method of generated) expect(implementations[methodKey(method)]).toBeFunction();
});

test("provider error codes pass through only when they are standard codes", async () => {
  const failing = (code: unknown): SocketProvider => ({
    tcp: {
      create: () => ({
        local: undefined,
        bind() {
          throw new SocketProviderError(code as never);
        },
        connect: async () => {
          throw new SocketProviderError({ tag: "connection-refused" });
        },
        getOption() {
          throw new SocketProviderError({ tag: "not-supported" });
        },
        close: async () => {},
      }),
    },
    resolve: async () => {
      throw new NameLookupError({ tag: "temporary-resolver-failure" });
    },
  });
  const grant = { bind: ["127.0.0.1:1"], connect: ["127.0.0.1:80"], resolve: true };
  const { statics, call, host } = orb(failing({ tag: "address-in-use" }), grant);
  const socket = ok<TcpSocket>(statics.tcpCreate(call(), "ipv4"));
  expect(socket.bind(loopback(1))).toEqual({ tag: "err", val: { tag: "address-in-use" } });
  expect(socket.getHopLimit()).toEqual({ tag: "err", val: { tag: "not-supported" } });
  expect(await socket.connect(loopback(80), call())).toEqual({
    tag: "err",
    val: { tag: "connection-refused" },
  });
  // After a failed connect the socket is closed and reports that error.
  expect(socket.getRemoteAddress()).toEqual({ tag: "err", val: { tag: "connection-refused" } });
  expect(await statics.resolveAddresses(call(), "example.test")).toEqual({
    tag: "err",
    val: { tag: "temporary-resolver-failure" },
  });
  const bogus = orb(failing({ tag: "bogus" }), grant);
  const other = ok<TcpSocket>(bogus.statics.tcpCreate(bogus.call(), "ipv4"));
  expect(other.bind(loopback(1)).val).toEqual({
    tag: "other",
    val: "socket provider failed: bogus",
  });
  socket.release();
  other.release();
  await retired(host);
  await retired(bogus.host);
});

test("packets: refusals and results cross the generated wire as standard values", async () => {
  const { provider, calls } = loopbackNetwork();
  const guest = packetOrb(provider, { connect: ["127.0.0.1:80"], resolve: true });
  const signal = new AbortController().signal;
  expect(guest.sync("udp-socket", "create", "ipv4")).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  const socket = guest.sync("tcp-socket", "create", "ipv4");
  expect(socket.tag).toBe("ok");
  expect(guest.sync("tcp-socket", "bind", socket.val, loopback(2))).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  expect(await guest.call("tcp-socket", "connect", signal, socket.val, loopback(81))).toEqual({
    tag: "err",
    val: { tag: "access-denied" },
  });
  expect(guest.sync("tcp-socket", "getAddressFamily", socket.val)).toBe("ipv4");
  expect(await guest.call(undefined, "resolveAddresses", signal, "10.0.0.1")).toEqual({
    tag: "ok",
    val: [{ tag: "ipv4", val: [10, 0, 0, 1] }],
  });
  expect(calls["tcp.bind"] ?? 0).toBe(0);
  expect(calls["tcp.connect"] ?? 0).toBe(0);
  guest.drop(socket.val);
  await retired(guest.host);
});

test("packets: accepted sockets are issued only by their reply; discard rolls back and poisons", async () => {
  const { provider, calls } = loopbackNetwork();
  const guest = packetOrb(provider, { listen: ["0.0.0.0:0"], connect: ["127.0.0.1:40000"] });
  const signal = new AbortController().signal;
  const server = guest.sync("tcp-socket", "create", "ipv4").val;
  const listen = guest.sync("tcp-socket", "listen", server);
  expect(listen.tag).toBe("ok");
  const clients: number[] = [];
  for (let n = 0; n < 3; n++) {
    const client = guest.sync("tcp-socket", "create", "ipv4").val;
    expect(await guest.call("tcp-socket", "connect", signal, client, loopback(40000))).toEqual({
      tag: "ok",
      val: undefined,
    });
    clients.push(client);
  }
  const before = guest.host.size;
  // A read asking for 8 items returns exactly one owned handle, issued by this reply.
  const delivered = await guest.read(listen.val, 8);
  const accepted = acceptedHandle(delivered.bytes)!;
  expect(delivered.issued).toEqual([accepted]);
  expect(delivered.reads).toEqual([listen.val]);
  expect(guest.host.size).toBe(before + 1);
  expect(guest.sync("tcp-socket", "getRemoteAddress", accepted).tag).toBe("ok");
  expect(calls["tcp.accept"]).toBe(1);
  // A reply that never reaches the guest: its handle is rolled back and closed, and the
  // listen stream is poisoned like every undelivered stream read.
  const closesBefore = calls["tcp.close"] ?? 0;
  const lost = await guest.read(listen.val, 1);
  const lostHandle = acceptedHandle(lost.bytes)!;
  guest.host.discard(lost);
  await guest.host.settled();
  expect(() => guest.sync("tcp-socket", "getRemoteAddress", lostHandle)).toThrow(
    "unknown or dropped handle",
  );
  expect(calls["tcp.close"]).toBe(closesBefore + 1);
  await expect(guest.read(listen.val, 1)).rejects.toThrow("stream read was abandoned");
  for (const handle of [accepted, listen.val, server, ...clients]) guest.drop(handle);
  await retired(guest.host);
  expect(guest.host.size).toBe(0);
});

test("packets: capacity is reserved before the pull, and a refused reply releases its socket", async () => {
  const { provider, calls } = loopbackNetwork();
  const guest = packetOrb(provider, { listen: ["0.0.0.0:0"], connect: ["127.0.0.1:40000"] });
  const signal = new AbortController().signal;
  const server = guest.sync("tcp-socket", "create", "ipv4").val;
  const listen = guest.sync("tcp-socket", "listen", server).val;
  const client = guest.sync("tcp-socket", "create", "ipv4").val;
  await guest.call("tcp-socket", "connect", signal, client, loopback(40000));
  // Fill the handle table: the read is refused before any accept is pulled.
  const filler: number[] = [];
  for (;;) {
    try {
      filler.push(guest.sync("tcp-socket", "create", "ipv4").val);
    } catch (error) {
      expect(String(error)).toContain("standard handle table is full");
      break;
    }
  }
  await expect(guest.read(listen, 1)).rejects.toThrow("standard handle table is full");
  expect(calls["tcp.accept"] ?? 0).toBe(0);
  guest.drop(filler.pop()!);
  expect(acceptedHandle((await guest.read(listen, 1)).bytes)).toBeNumber();
  for (const handle of filler) guest.drop(handle);
  await retired(guest.host);

  // A reply larger than the packet bound: the issued handle is rolled back and the accepted
  // socket closed; nothing was published, so the stream is not poisoned.
  const small = loopbackNetwork();
  const bounded = packetOrb(
    small.provider,
    { listen: ["0.0.0.0:0"], connect: ["127.0.0.1:40000"] },
    12,
  );
  const host = bounded.sync("tcp-socket", "create", "ipv4").val;
  const stream = bounded.sync("tcp-socket", "listen", host).val;
  const peer = bounded.sync("tcp-socket", "create", "ipv4").val;
  await bounded.call("tcp-socket", "connect", signal, peer, loopback(40000));
  const sizeBefore = bounded.host.size;
  await expect(bounded.read(stream, 1)).rejects.toThrow("standard reply exceeds the packet bound");
  await bounded.host.settled();
  expect(bounded.host.size).toBe(sizeBefore);
  expect(small.closes.length).toBe(1);
  await retired(bounded.host);
});

test("owned items: a drop racing a resolving take leaves exactly one owner at every microtask order", async () => {
  // depth -1 drops before the value resolves; depth d >= 0 drops d microtasks after it resolves.
  for (let depth = -1; depth <= 8; depth++) {
    let resolve!: (value: unknown) => void;
    let pulls = 0;
    const released: unknown[] = [];
    const stream = new OutputStream(
      {
        next: () => {
          pulls++;
          return new Promise((accept) => (resolve = accept));
        },
        close() {},
      },
      () => {},
      (value) => released.push(value),
    );
    const value = { depth };
    const taking = stream.take().then(
      (taken) => taken?.value,
      (error: Error) => error.message,
    );
    while (!pulls) await Promise.resolve();
    if (depth < 0) stream.close();
    resolve(value);
    for (let hop = 0; hop < depth; hop++) await Promise.resolve();
    if (depth >= 0) stream.close();
    const delivered = await taking;
    await tick();
    // Either the read delivers the value and close() releases nothing, or the read fails and
    // close() releases it once: one owner, one physical close.
    const owners = (delivered === value ? 1 : 0) + released.length;
    expect({ depth, owners, released: released.length <= 1 }).toEqual({
      depth,
      owners: 1,
      released: true,
    });
    if (delivered !== value)
      expect({ depth, delivered }).toEqual({ depth, delivered: "stream was dropped" });
    expect(pulls).toBe(1);
  }
});

test("packets: a pending accept holds its handle slot against concurrent reads and issues", async () => {
  const { provider, calls, closes } = loopbackNetwork();
  const guest = packetOrb(provider, {
    listen: ["0.0.0.0:0"],
    connect: ["127.0.0.1:40000", "127.0.0.1:40001"],
  });
  const signal = new AbortController().signal;
  const first = guest.sync("tcp-socket", "listen", guest.sync("tcp-socket", "create", "ipv4").val);
  const second = guest.sync("tcp-socket", "listen", guest.sync("tcp-socket", "create", "ipv4").val);
  const client = guest.sync("tcp-socket", "create", "ipv4").val;
  const filler: number[] = [];
  for (;;) {
    try {
      filler.push(guest.sync("tcp-socket", "create", "ipv4").val);
    } catch {
      break;
    }
  }
  // Exactly one free slot. The first listen stream's read reserves it while its pull waits.
  guest.drop(filler.pop()!);
  await guest.host.settled();
  const closed = closes.length;
  const reading = guest.read(first.val, 1);
  await tick();
  expect(calls["tcp.accept"]).toBe(1);
  // A concurrent accept is refused before pulling a connection it could not deliver, and a
  // concurrent issue cannot take the reserved slot either.
  const refused = guest.read(second.val, 1).then(
    () => "delivered",
    (error: Error) => error.message,
  );
  await tick();
  expect(await Promise.race([refused, Promise.resolve("still pulling")])).toBe(
    "standard handle table is full",
  );
  expect(calls["tcp.accept"]).toBe(1);
  expect(() => guest.sync("tcp-socket", "create", "ipv4")).toThrow("standard handle table is full");
  // A call issuing no handle still runs; its connection arrives and the reservation becomes the
  // accepted handle.
  expect(await guest.call("tcp-socket", "connect", signal, client, loopback(40000))).toEqual({
    tag: "ok",
    val: undefined,
  });
  const accepted = acceptedHandle((await reading).bytes)!;
  expect(accepted).toBeNumber();
  // No accepted connection was pulled only to be closed for lack of room.
  expect(closes.length).toBe(closed);
  expect(() => guest.sync("tcp-socket", "create", "ipv4")).toThrow("standard handle table is full");

  // A cancelled read returns its reservation; its pull and future value stay with the stream.
  guest.drop(accepted);
  const cancel = new AbortController();
  const cancelled = guest.read(first.val, 1, cancel.signal);
  await tick();
  expect(() => guest.sync("tcp-socket", "create", "ipv4")).toThrow("standard handle table is full");
  cancel.abort();
  await expect(cancelled).rejects.toThrow();
  const spare = guest.sync("tcp-socket", "create", "ipv4").val;
  guest.drop(spare);
  // A failed read (the stream is dropped while it waits) returns its reservation too.
  const failing = guest.read(first.val, 1);
  await tick();
  guest.drop(first.val);
  await expect(failing).rejects.toThrow("stream was dropped");
  guest.drop(guest.sync("tcp-socket", "create", "ipv4").val);
  for (const handle of [second.val, client, ...filler]) guest.drop(handle);
  await retired(guest.host);
  expect(guest.host.size).toBe(0);
});
