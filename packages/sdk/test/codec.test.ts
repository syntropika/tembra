import { expect, test } from "bun:test";
import { jsonCodec, text, utf8 } from "../src/codec.ts";
import { protect } from "../src/values.ts";
import { messagePackCodec } from "../src/msgpack.ts";

test("MessagePack preserves BigInt at every size", () => {
  const input = [1n, -(1n << 100n), 1n << 120n, new Uint8Array([1, 2])];
  expect(messagePackCodec.deserialize(messagePackCodec.serialize(input))).toEqual(input);
});

test("JSON codec preserves BigInt, bytes, strings and marker collisions", () => {
  const input = {
    big: 18446744073709551615n,
    bytes: new Uint8Array([0, 255]),
    text: "00123",
    nested: { $tembra: ["bigint", "5"] },
  };
  expect(jsonCodec.deserialize(jsonCodec.serialize(input))).toEqual(input);
  expect(() => jsonCodec.serialize({ value: undefined })).toThrow();
  expect(() => jsonCodec.serialize(Number.MAX_SAFE_INTEGER + 1)).toThrow();
});

test("UTF-8 handles Unicode and rejects malformed input", () => {
  expect(text(utf8("A😀é中"))).toBe("A😀é中");
  expect(() => text(new Uint8Array([0xc0, 0x80]))).toThrow();
  expect(() => text(new Uint8Array([0xed, 0xa0, 0x80]))).toThrow();
});

test("external shallow freezing does not bypass recursive state protection", () => {
  const child = { value: 1 };
  const root = Object.freeze({ child });
  protect(root);
  expect(Object.isFrozen(child)).toBe(true);
  expect(() => protect(new Map())).toThrow();
  const cycle: Record<string, unknown> = {};
  cycle.self = cycle;
  expect(() => protect(cycle)).toThrow();
});
