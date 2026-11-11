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

test("Context.get acquisition names only an import binding and a key list", () => {
  expect(
    decodeAsyncHostRequest('{"operation":"reference.acquire","service":"s","key":["a","b"]}'),
  ).toEqual({ operation: "reference.acquire", service: "s", key: ["a", "b"] });
  // Wrong key kinds, a table selector, and guest-chosen requirement or identity are refused.
  for (const packet of [
    '{"operation":"reference.acquire","service":"s","key":"main"}',
    '{"operation":"reference.acquire","service":"s","key":[1]}',
    '{"operation":"reference.acquire","service":"s"}',
    '{"operation":"reference.acquire","scope":1,"service":"s","key":[]}',
    '{"operation":"reference.acquire","service":"s","key":[],"apiVersion":2}',
    '{"operation":"reference.acquire","service":"s","key":[],"methods":["m"]}',
    '{"operation":"reference.acquire","service":"s","key":[],"principal":"p"}',
  ])
    expect(outcome(decodeAsyncHostRequest, packet)).toBe("malformed");
});

test("component calls preserve their import binding and reject malformed identities", () => {
  const call = { operation: "reference.call" as const, id: 1, method: "run", args: [] };
  expect(
    decodeAsyncHostRequest(JSON.stringify({ ...call, binding: "example:app/service@1.0.0" })),
  ).toEqual({ ...call, binding: "example:app/service@1.0.0" });
  expect(outcome(decodeAsyncHostRequest, JSON.stringify(call))).toBe("malformed");
  for (const binding of [null, 1, true, {}, []])
    expect(outcome(decodeAsyncHostRequest, JSON.stringify({ ...call, binding }))).toBe("malformed");
});
