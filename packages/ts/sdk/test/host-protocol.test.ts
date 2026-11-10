import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { decodeAsyncHostRequest, decodeHostRequest } from "../src/interpreter/host-protocol.ts";

/** Classify a packet as decoded, malformed or unsupported without depending on full messages. */
function outcome(decode: (packet: string) => unknown, packet: string): string {
  try {
    decode(packet);
    return "ok";
  } catch (error) {
    const message = (error as Error).message;
    if (message.startsWith("Unsupported")) return "unsupported";
    if (message.startsWith("Malformed")) return "malformed";
    throw error;
  }
}

test("the SDK and the native executor share one host-operation vocabulary", () => {
  const source = readFileSync(
    new URL("../../../../crates/executor-wasm/src/protocol.rs", import.meta.url),
    "utf8",
  );
  const block = (name: string) =>
    source.slice(source.indexOf(`pub enum ${name} {`)).split("\n}\n")[0]!;
  const operations = (name: string) =>
    [...block(name).matchAll(/rename = "([^"]+)"/g)].map((match) => match[1]!);
  const sync = operations("Request");
  const asynchronous = operations("AsyncRequest");
  expect(sync).not.toContain("io.check");
  expect(asynchronous).toEqual(
    expect.arrayContaining([
      "transport.connect",
      "transport.read",
      "transport.write",
      "transport.close",
    ]),
  );
  expect(sync).not.toContain("filesystem");
  expect(sync).not.toContain("network");
  for (const operation of sync) {
    const packet = JSON.stringify({ operation });
    expect(outcome(decodeHostRequest, packet)).not.toBe("unsupported");
    expect(outcome(decodeAsyncHostRequest, packet)).toBe("unsupported");
  }
  for (const operation of asynchronous) {
    const packet = JSON.stringify({ operation });
    expect(outcome(decodeAsyncHostRequest, packet)).not.toBe("unsupported");
    expect(outcome(decodeHostRequest, packet)).toBe("unsupported");
  }
});

test("typed requests normalize optional fields", () => {
  expect(decodeHostRequest('{"operation":"state.declare","version":1}')).toEqual({
    operation: "state.declare",
    initial: null,
    version: 1,
    validate: null,
  });
  expect(decodeHostRequest('{"operation":"state.read","id":2}')).toEqual({
    operation: "state.read",
    id: 2,
  });
  expect(
    decodeHostRequest(
      '{"operation":"resource.declare","name":"r","version":1,"open":1,"close":2,"dependencies":[{"scope":4,"id":9}]}',
    ),
  ).toEqual({
    operation: "resource.declare",
    name: "r",
    version: 1,
    open: 1,
    close: 2,
    dependencies: [{ scope: 4, id: 9 }],
  });
});

test("malformed and unsupported requests are distinguished", () => {
  for (const packet of [
    "not json",
    '{"id":1}',
    '{"operation":"state.read"}',
    '{"operation":"state.read","id":-1}',
    '{"operation":"state.read","id":1.5}',
    '{"operation":"state.read","id":4294967296}',
    '{"operation":"state.read","id":1,"scope":"7"}',
    '{"operation":"state.read","id":1,"context":3}',
    '{"operation":"address","scope":1}',
    '{"operation":"state.declare","scope":1,"version":1}',
    '{"operation":"reference.declare","service":"s","key":"main"}',
    '{"operation":"resource.declare","name":"r","version":1,"open":1,"close":2,"dependencies":[{"id":1,"extra":0}]}',
    '{"operation":"console","level":"trace","message":"x"}',
  ])
    expect(outcome(decodeHostRequest, packet)).toBe("malformed");
  for (const packet of [
    '{"operation":"filesystem"}',
    '{"operation":"network"}',
    '{"operation":"reference.call","id":1,"method":"m","args":[]}',
  ])
    expect(outcome(decodeHostRequest, packet)).toBe("unsupported");
  expect(outcome(decodeAsyncHostRequest, '{"operation":"wait","milliseconds":-1}')).toBe(
    "malformed",
  );
  expect(
    outcome(decodeAsyncHostRequest, '{"operation":"reference.call","id":1,"method":"m"}'),
  ).toBe("malformed");
});

test("host I/O packets reject forged context, invalid ports and non-byte chunks", () => {
  for (const packet of [
    { operation: "transport.read", id: 1, context: 1 },
    { operation: "transport.write", id: 1 },
    { operation: "transport.connect", ip: "127.0.0.1", port: 65536 },
    { operation: "transport.connect", ip: "127.0.0.1", port: -1 },
    { operation: "transport.read", id: -1 },
    { operation: "transport.read", id: 1, scope: 2 },
    { operation: "transport.write", id: 1, bytes: [256] },
    { operation: "transport.write", id: 1, bytes: [-1] },
    { operation: "transport.write", id: 1, bytes: [1.5] },
    { operation: "transport.write", id: 1, bytes: "ab" },
    { operation: "transport.write", id: 1, bytes: Array.from({ length: 16 * 1024 + 1 }, () => 0) },
  ])
    expect(outcome(decodeAsyncHostRequest, JSON.stringify(packet))).toBe("malformed");
  expect(
    decodeAsyncHostRequest(
      JSON.stringify({ operation: "transport.write", id: 1, bytes: [0, 255] }),
    ),
  ).toEqual({ operation: "transport.write", id: 1, bytes: [0, 255] });
});
