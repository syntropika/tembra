import { expect, test } from "bun:test";
import { jsonCodec, text, utf8 } from "../src/codec.ts";
import { protect } from "../src/values.ts";
import { messagePackCodec } from "../src/msgpack.ts";

test("MessagePack preserves BigInt at every size", () => {
  const input = [
    0n,
    1n,
    -1n,
    BigInt(Number.MAX_SAFE_INTEGER),
    BigInt(Number.MAX_SAFE_INTEGER) + 1n,
    -(1n << 63n),
    (1n << 64n) - 1n,
    1n << 64n,
    -(1n << 100n),
    1n << 120n,
    new Uint8Array([1, 2]),
  ];
  expect(messagePackCodec.deserialize(messagePackCodec.serialize(input))).toEqual(input);
});

test("MessagePack keeps safe Numbers distinct from BigInts at integer encoding boundaries", () => {
  const numbers = [
    0,
    1,
    -1,
    0.5,
    2 ** 31 - 1,
    2 ** 31,
    -(2 ** 31),
    -(2 ** 31) - 1,
    2 ** 32 - 1,
    2 ** 32,
    2 ** 32 + 1,
    -(2 ** 32),
    -(2 ** 32) - 1,
    1_760_000_000_123,
    Number.MAX_SAFE_INTEGER - 1,
    Number.MAX_SAFE_INTEGER,
    Number.MIN_SAFE_INTEGER,
    Number.MIN_SAFE_INTEGER + 1,
  ];
  for (const value of numbers) {
    expect(messagePackCodec.deserialize(messagePackCodec.serialize(value))).toBe(value);
  }
  const input = { numbers, nested: { pair: [2 ** 32, BigInt(2 ** 32)] } };
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

test("portable BigInt decimal text matches the Rust integer contract", () => {
  for (const decimal of ["0", "-0", "7", "-7", "9007199254740993", "-18446744073709551616"])
    expect(jsonCodec.deserialize(utf8(JSON.stringify({ $tembra: ["bigint", decimal] })))).toBe(
      BigInt(decimal),
    );
  for (const decimal of ["", "-", "+7", "007", "-007", "00", "-00", "1.0", " 7", "７"])
    expect(() =>
      jsonCodec.deserialize(utf8(JSON.stringify({ $tembra: ["bigint", decimal] }))),
    ).toThrow();
});
