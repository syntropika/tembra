import { expect, test } from "bun:test";
import type { MobileIngressEvent } from "@syntropika/tembra-sdk/runtime";
import type { Spec } from "../src/NativeTembra.ts";
import { MobileExecution } from "../src/execution.ts";

/** A native double that keeps one pending buffer per port, refusing out-of-order use like Swift/Kotlin. */
function bridge(events: { metadata: unknown; bytes?: Uint8Array }[]) {
  const calls: { method: string; metadata?: unknown; bytes?: number[] }[] = [];
  let pending: ArrayBuffer | undefined;
  const native = {
    configureExecution: async () => {},
    createArtifact: () => "artifact",
    openPort: async () => "port",
    portLogs: () => "[]",
    portBeginIngress(_h: string, _p: string, packet: string, head: string, fields: ArrayBuffer) {
      calls.push({
        method: "begin",
        metadata: { packet, head: JSON.parse(head) },
        bytes: [...new Uint8Array(fields)],
      });
      return true;
    },
    async portReceiveIngress() {
      if (pending) throw new Error("The previous ingress event has not been taken");
      const next = events.shift();
      if (!next) throw new Error("no event");
      if (next.bytes) pending = next.bytes.slice().buffer;
      return JSON.stringify(next.metadata);
    },
    portTakeIngress() {
      if (!pending) throw new Error("No received ingress event has pending bytes");
      const bytes = pending;
      pending = undefined;
      calls.push({ method: "take", bytes: [...new Uint8Array(bytes)] });
      return bytes;
    },
    portSupplyRequest(_h: string, _p: string, chunk: string, bytes: ArrayBuffer) {
      calls.push({
        method: "supply",
        metadata: JSON.parse(chunk),
        bytes: [...new Uint8Array(bytes)],
      });
      return true;
    },
    portPullResponse: () => (calls.push({ method: "pull" }), true),
    portDisconnect: () => (calls.push({ method: "disconnect" }), true),
  };
  return { native, calls, pending: () => pending };
}

async function open(native: object) {
  const execution = new MobileExecution(native as unknown as Spec, { handle: "host" } as never, {});
  return execution.open(execution.artifact(new Uint8Array([0])), "{}", "{}", "{}");
}

const field = (name: string, ...value: number[]) => ({ name, value: Uint8Array.from(value) });

test("begins with field names in metadata and raw ordered values in one buffer", async () => {
  const { native, calls } = bridge([]);
  const port = await open(native);
  port.ingress!.beginIngress('{"kind":"ingress"}', {
    method: "PATCH",
    scheme: "https",
    authority: "app.example.test",
    pathWithQuery: "/echo",
    fields: [field("x-raw", 0x80, 0xff), field("x-dup", 1), field("x-dup", 2, 3), field("x-empty")],
  });
  expect(calls).toEqual([
    {
      method: "begin",
      metadata: {
        packet: '{"kind":"ingress"}',
        head: {
          method: "PATCH",
          scheme: "https",
          authority: "app.example.test",
          pathWithQuery: "/echo",
          fields: [
            ["x-raw", 2],
            ["x-dup", 1],
            ["x-dup", 2],
            ["x-empty", 0],
          ],
        },
      },
      bytes: [0x80, 0xff, 1, 2, 3],
    },
  ]);
});

test("takes exactly the received byte event's buffer and nothing for control events", async () => {
  const { native, calls, pending } = bridge([
    { metadata: { kind: "host", id: 3, asynchronous: true, packet: "{}" } },
    {
      metadata: {
        kind: "head",
        status: 200,
        fields: [
          ["set-cookie", 1],
          ["set-cookie", 2],
          ["x-bin", 2],
        ],
      },
      bytes: Uint8Array.of(97, 98, 99, 0, 255),
    },
    { metadata: { kind: "requestPull" } },
    { metadata: { kind: "data", length: 4 }, bytes: Uint8Array.of(0, 1, 254, 255) },
    { metadata: { kind: "trailers", fields: [["x-ticks", 1]] }, bytes: Uint8Array.of(49) },
    { metadata: { kind: "end" } },
    { metadata: { kind: "error", message: "broken" } },
    { metadata: { kind: "refused", length: 3 }, bytes: Uint8Array.of(0, 0x80, 0xff) },
    { metadata: { kind: "complete", result: '{"ok":null}' } },
  ]);
  const ingress = (await open(native)).ingress!;
  const received: MobileIngressEvent[] = [];
  for (let i = 0; i < 9; i++) {
    received.push(await ingress.receiveIngress());
    expect(pending()).toBeUndefined();
  }
  const plain = JSON.parse(
    JSON.stringify(received, (_, value) => (value instanceof Uint8Array ? [...value] : value)),
  );
  expect(plain).toEqual([
    { kind: "host", id: 3, asynchronous: true, packet: "{}" },
    {
      kind: "head",
      status: 200,
      fields: [
        { name: "set-cookie", value: [97] },
        { name: "set-cookie", value: [98, 99] },
        { name: "x-bin", value: [0, 255] },
      ],
    },
    { kind: "requestPull" },
    { kind: "data", bytes: [0, 1, 254, 255] },
    { kind: "trailers", fields: [{ name: "x-ticks", value: [49] }] },
    { kind: "end" },
    { kind: "error", message: "broken" },
    { kind: "refused", code: [0, 0x80, 0xff] },
    { kind: "complete", result: '{"ok":null}' },
  ]);
  expect(received[3]!.kind === "data" && received[3]!.bytes instanceof Uint8Array).toBe(true);
  expect(calls.filter((call) => call.method === "take").length).toBe(4);
});

test("refuses metadata that does not describe the taken bytes", async () => {
  for (const metadata of [
    { kind: "data", length: 3 },
    { kind: "refused", length: 1 },
    { kind: "head", status: 200, fields: [["a", 1]] },
    { kind: "trailers", fields: [["a", 3]] },
    { kind: "trailers", fields: [["a", -1]] },
  ]) {
    const { native, pending } = bridge([{ metadata, bytes: Uint8Array.of(1, 2) }]);
    const ingress = (await open(native)).ingress!;
    await expect(ingress.receiveIngress()).rejects.toThrow(TypeError);
    // The buffer was still taken: a malformed event never strands the port's single slot.
    expect(pending()).toBeUndefined();
  }
});

test("supplies the exact bytes of partial views, trailers and errors", async () => {
  const { native, calls } = bridge([]);
  const ingress = (await open(native)).ingress!;
  const whole = Uint8Array.from({ length: 10 }, (_, index) => index);
  expect(ingress.supplyRequest({ kind: "data", bytes: whole.subarray(3, 6) })).toBe(true);
  ingress.supplyRequest({ kind: "data", bytes: whole });
  ingress.supplyRequest({ kind: "end", trailers: [field("x-a", 0x80), field("x-a", 0xff, 0)] });
  ingress.supplyRequest({ kind: "error", message: "client failed" });
  ingress.pullResponse();
  ingress.disconnect();
  expect(calls).toEqual([
    { method: "supply", metadata: { kind: "data" }, bytes: [3, 4, 5] },
    { method: "supply", metadata: { kind: "data" }, bytes: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9] },
    {
      method: "supply",
      metadata: {
        kind: "end",
        trailers: [
          ["x-a", 1],
          ["x-a", 2],
        ],
      },
      bytes: [0x80, 0xff, 0],
    },
    { method: "supply", metadata: { kind: "error", message: "client failed" }, bytes: [] },
    { method: "pull" },
    { method: "disconnect" },
  ]);
});

test("does not declare ingress unless the whole native group is present", async () => {
  const { native } = bridge([]);
  const { portTakeIngress: _, ...partial } = native;
  expect((await open(partial)).ingress).toBeUndefined();
  expect((await open(native)).ingress).toBeDefined();
});
