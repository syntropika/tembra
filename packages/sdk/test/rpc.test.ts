import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { RemoteHost, serve } from "../src/rpc.ts";
import type { Transport } from "../src/rpc.ts";
import { messagePackCodec } from "../src/msgpack.ts";
import { jsonCodec } from "../src/codec.ts";

function pair(): [Transport, Transport] {
  const endpoints = [0, 1].map(() => ({
    receive: (_frame: Uint8Array) => {},
    ended: (_error?: unknown) => {},
    closed: false,
  }));
  return endpoints.map((endpoint, index) => ({
    send(frame: Uint8Array) {
      const copied = frame.slice();
      queueMicrotask(() => {
        if (!endpoint.closed) endpoints[1 - index]!.receive(copied);
      });
    },
    listen(receive: (frame: Uint8Array) => void, ended: (error?: unknown) => void) {
      endpoint.receive = receive;
      endpoint.ended = ended;
      return () => {
        endpoint.receive = () => {};
      };
    },
    close() {
      if (endpoint.closed) return;
      for (const endpoint of endpoints) {
        endpoint.closed = true;
        endpoint.ended();
      }
    },
  })) as [Transport, Transport];
}

for (const codec of [undefined, messagePackCodec])
  test(`remote calls, immutable state and disconnect using ${codec?.id ?? "json"}`, async () => {
    const host = createHost();
    await host.install(
      facet({
        name: "counter",
        apiVersion: 1,
        services: {
          counter: {
            create(self) {
              return { state: self.state({ value: 0n }) };
            },
            methods: {
              add({ state }, ctx, amount: bigint) {
                return state.update(ctx, (value: { value: bigint }) => ({
                  value: value.value + amount,
                })).value;
              },
            },
          },
        },
      }),
    );
    const [server, client] = pair();
    const options = codec ? { codec } : {};
    serve(host, server, { ...options, principal: "authenticated" });
    const remote = new RemoteHost(client, options);
    const ref = await remote.get("counter", "main");
    const values: bigint[] = [];
    const detach = ref.state
      .select<bigint>((value) => value.value)
      .subscribe((value) => values.push(value));
    expect(await ref.add(1n << 100n)).toBe(1n << 100n);
    await Promise.resolve();
    expect(ref.state.value.value).toBe(1n << 100n);
    expect(values).toEqual([0n, 1n << 100n]);
    expect(Object.isFrozen(ref.state.value)).toBe(true);
    detach();
    remote.close();
    expect(ref.status).toBe("gone");
    await expect(ref.add(1n)).rejects.toMatchObject({ code: "gone" });
    await host.close();
    expect(host.diagnostics().references).toBe(0);
  });

test("raw RPC calls expose only the reference's declared allowed methods", async () => {
  const host = createHost();
  await host.install(
    facet({
      name: "private",
      apiVersion: 1,
      services: {
        private: {
          create(self) {
            self.resource("db", { opaque: true, open: () => ({ secret: "opaque-secret" }) });
            return {};
          },
          access: () => ({ methods: ["read"] }),
          methods: { read: () => "public", admin: () => "private" },
        },
      },
    }),
  );
  const [server, client] = pair();
  const sent: unknown[] = [];
  const send = server.send.bind(server);
  server.send = (frame) => {
    sent.push(jsonCodec.deserialize(frame));
    return send(frame);
  };
  serve(host, server, { principal: "authenticated" });
  const remote = new RemoteHost(client);
  try {
    const acquired = await remote.request("get", { service: "private", key: ["main"] });
    const outcomes = await Promise.allSettled(
      ["opaque", "hasOwnProperty", "constructor", "valueOf", "toString", "admin"].map((method) =>
        remote.request("call", {
          reference: acquired.reference,
          method,
          arguments: method === "opaque" ? ["db"] : [],
        }),
      ),
    );
    for (const outcome of outcomes)
      expect(outcome).toMatchObject({
        status: "rejected",
        reason: { code: "forbidden", reason: "method" },
      });
    expect(acquired.methods).toEqual(["read"]);
    expect(JSON.stringify(sent)).not.toContain("opaque-secret");
    expect(
      await remote.request("call", {
        reference: acquired.reference,
        method: "read",
        arguments: [],
      }),
    ).toBe("public");
  } finally {
    remote.close();
    await host.close();
  }
});
