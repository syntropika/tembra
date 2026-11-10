// WIT u64/s64 in the hardened host capture: one read-once walk lowers `bigint` to canonical
// decimal text exactly where the prepared signature says so, and refuses every other kind there.
// The graph is the real contract `describe` answer for the WIT below.
import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { NodeContract } from "@syntropika/tembra-engine-node";
import { encode, VALUE_DEPTH } from "../src/interpreter/codec.ts";
import {
  integerText,
  integerValue,
  lift,
  shapes,
  type Signature,
  type WitFunction,
  type WitTypes,
} from "../src/interpreter/wit-shape.ts";

const WIT = `package example:precision;
interface numbers {
  type big = u64;
  record rec { id: big, values: list<s64>, maybe: option<u64>, small: u32, label: string }
  variant pick { signed(s64), none }
  add: func(a: u64, b: s64) -> u64;
  echo: func(r: rec, p: pick, t: tuple<u64, string>) -> result<u64, s64>;
  plain: func(x: u32, s: string, l: list<u32>) -> string;
  wide: func(u: list<u64>, s: list<s64>) -> list<u64>;
}
world test { export numbers; }`;

const MAX = (1n << 64n) - 1n;
const MIN = -(1n << 63n);
const SMAX = (1n << 63n) - 1n;

const directory = await mkdtemp(path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "wit-"));
await writeFile(path.join(directory, "contract.wit"), WIT);
const graph = JSON.parse(new NodeContract(directory).request('{"kind":"describe"}'))
  .ok as WitTypes & {
  exports: { functions: Record<string, WitFunction> }[];
};
await rm(directory, { recursive: true, force: true });
const compile = shapes(graph);
const fn = (name: string): Signature => compile(graph.exports[0]!.functions[name]!);
const add = fn("add");
const echo = fn("echo");
const plain = fn("plain");
const wide = fn("wide");
/** Capture with `shape`, which defaults to `echo`'s arguments only when not passed at all. */
const text = (value: unknown, ...shape: [Signature["params"]?]) =>
  encode(value, undefined, shape.length ? shape[0] : echo.params).text;
const refusal = (value: unknown, ...shape: [Signature["params"]?]) => {
  try {
    text(value, ...shape);
  } catch (error) {
    return String((error as Error).message);
  }
  throw new Error("expected a refusal");
};
const record = (overrides: Record<string, unknown> = {}) => ({
  id: MAX,
  values: new BigInt64Array([MIN, -1n, 0n, SMAX]),
  maybe: undefined,
  small: 7,
  label: "123",
  ...overrides,
});

test("integer helpers: strict bigint in, canonical decimal text out, and back", () => {
  for (const [kind, value, wire] of [
    ["u64", 0n, "0"],
    ["u64", 1n, "1"],
    ["u64", (1n << 53n) + 1n, "9007199254740993"],
    ["u64", MAX, "18446744073709551615"],
    ["s64", MIN, "-9223372036854775808"],
    ["s64", SMAX, "9223372036854775807"],
    ["s64", -1n, "-1"],
  ] as const) {
    expect(integerText(kind, value)).toBe(wire);
    expect(integerValue(kind, wire)).toBe(value);
  }
  for (const value of [1, "1", true, null, undefined, Object(1n), MAX + 1n, -1n])
    expect(() => integerText("u64", value)).toThrow();
  for (const value of [SMAX + 1n, MIN - 1n]) expect(() => integerText("s64", value)).toThrow();
  for (const wire of ["01", "-0", "+1", " 1", "1 ", "1e3", "0x1", "", "18446744073709551616", "-1"])
    expect(() => integerValue("u64", wire)).toThrow();
  for (const wire of ["9223372036854775808", "-9223372036854775809", "-01", 1])
    expect(() => integerValue("s64", wire)).toThrow();
});

test("shapes mark only 64-bit positions; 64-bit-free signatures keep the untyped capture", () => {
  expect(plain).toEqual({ params: undefined, result: undefined });
  expect(add.params).toEqual({ items: ["u64", "s64"] });
  expect(add.result).toBe("u64");
  // Through the `big` alias, a record, a list, an option, a variant, a tuple and a result.
  const [rec, pick, tuple] = (echo.params as { items: any[] }).items;
  expect(rec.fields.get("id")).toBe("u64");
  expect(rec.fields.get("values")).toEqual({ list: "s64" });
  expect(rec.fields.get("maybe")).toEqual({ option: "u64" });
  expect(rec.fields.has("label") && rec.fields.get("label") === undefined).toBe(true);
  expect(pick.cases.get("signed")).toBe("s64");
  expect(tuple.items).toEqual(["u64", undefined]);
  expect((echo.result as any).cases.get("err")).toBe("s64");
  // Without a shape the capture is today's: identical text for identical input.
  const value = [7, "x", [1, 2]];
  expect(text(value, plain.params)).toBe(encode(value).text);
});

test("typed capture writes exact canonical text through nested containers", () => {
  expect(text([0n, -1n], add.params)).toBe('["0","-1"]');
  expect(text([MAX, MIN], add.params)).toBe('["18446744073709551615","-9223372036854775808"]');
  expect(text([record(), { tag: "signed", val: -5n }, [1n, "2"]])).toBe(
    '[{"id":"18446744073709551615","values":["-9223372036854775808","-1","0","9223372036854775807"],"maybe":null,"small":7,"label":"123"},{"tag":"signed","val":"-5"},["1","2"]]',
  );
  // `val` before `tag` is still typed by the tag; a payload-free case keeps its shape.
  const reordered = text([record({ maybe: 3n }), { val: 4n, tag: "signed" }, [0n, ""]]);
  expect(reordered).toContain('"maybe":"3"');
  expect(reordered).toContain('{"val":"4","tag":"signed"}');
  expect(text([record({ values: [1n] }), { tag: "none" }, [0n, ""]])).toContain('"values":["1"]');
  // Typed arrays of the matching signedness, or arrays of bigint.
  expect(text([new BigUint64Array([MAX]), [MIN]], wide.params)).toBe(
    '[["18446744073709551615"],["-9223372036854775808"]]',
  );
  expect(encode(new BigUint64Array([1n, 2n]), undefined, wide.result).text).toBe('["1","2"]');
});

test("wrong kinds and limits are refused in 64-bit positions; strings stay strings", () => {
  for (const bad of [
    1,
    Number.MAX_SAFE_INTEGER + 2,
    "1",
    true,
    null,
    undefined,
    Object(1n),
    {},
    [1n],
  ])
    expect(refusal([bad, 0n], add.params)).toMatch(/u64 requires a bigint/);
  expect(refusal([MAX + 1n, 0n], add.params)).toMatch(/u64 value is out of range/);
  expect(refusal([-1n, 0n], add.params)).toMatch(/u64 value is out of range/);
  expect(refusal([0n, SMAX + 1n], add.params)).toMatch(/s64 value is out of range/);
  expect(refusal([0n, MIN - 1n], add.params)).toMatch(/s64 value is out of range/);
  // A literal digit string is a WIT string where WIT says string, never a u64.
  expect(text([record({ label: "18446744073709551615" }), { tag: "none" }, [0n, "1"]])).toContain(
    '"label":"18446744073709551615"',
  );
  expect(refusal([record(), { tag: "none" }, ["1", "1"]])).toMatch(/u64 requires a bigint/);
  // bigint outside a WIT 64-bit position stays refused, as before.
  expect(refusal([1n, "x", []], plain.params)).toMatch(/BigInt is portable only/);
  expect(refusal([record({ small: 7n }), { tag: "none" }, [0n, ""]])).toMatch(
    /BigInt is portable only/,
  );
  // Tuples, tags and records are checked only as far as typing needs them.
  expect(refusal([0n], add.params)).toMatch(/tuple length differs/);
  expect(refusal([record(), { tag: "nope" }, [0n, ""]])).toMatch(/known tag/);
  expect(refusal([record(), { tag: "signed", val: 1n, extra: 0 }, [0n, ""]])).toMatch(
    /only tag and val/,
  );
  expect(refusal([record({ extra: 1 }), { tag: "none" }, [0n, ""]])).toMatch(
    /field "extra" is not declared/,
  );
});

test("typed arrays: matching 64-bit arrays only, and never through a proxy", () => {
  expect(refusal([new BigInt64Array([1n]), []], wide.params)).toMatch(
    /list<u64> requires a BigUint64Array/,
  );
  expect(refusal([new Uint32Array([1]), []], wide.params)).toMatch(/BigUint64Array/);
  expect(refusal([[], new BigUint64Array([1n])], wide.params)).toMatch(/BigInt64Array/);
  expect(refusal([new Proxy(new BigUint64Array([1n]), {}), []], wide.params)).toBeTruthy();
  // 64-bit arrays outside a WIT 64-bit list stay refused.
  expect(refusal([1, "x", new BigUint64Array([1n])], plain.params)).toMatch(
    /64-bit lists are portable only/,
  );
  // Numeric arrays where WIT has a numeric list are unchanged.
  expect(text([1, "x", new Uint32Array([1, 2])], plain.params)).toBe('[1,"x",[1,2]]');
});

test("hostile input: one read per property, no getters, toJSON, cycles, depth or size escapes", () => {
  let getter = 0;
  const guarded = record();
  Object.defineProperty(guarded, "id", {
    enumerable: true,
    get() {
      getter++;
      return 1n;
    },
  });
  expect(refusal([guarded, { tag: "none" }, [0n, ""]])).toMatch(/accessors/);
  expect(getter).toBe(0);
  // Proxy traps: exactly one ownKeys and one descriptor read per key, typed or not.
  const traps = (shape: Signature["params"]) => {
    const counts = { ownKeys: 0, descriptor: 0, get: 0 };
    const target = { id: 1n, values: [], maybe: null, small: 1, label: "" };
    const proxy = new Proxy(target, {
      ownKeys(object) {
        counts.ownKeys++;
        return Reflect.ownKeys(object);
      },
      getOwnPropertyDescriptor(object, key) {
        counts.descriptor++;
        return Reflect.getOwnPropertyDescriptor(object, key);
      },
      get(object, key) {
        counts.get++;
        return Reflect.get(object, key);
      },
    });
    try {
      encode([proxy, { tag: "none" }, [0n, ""]], undefined, shape);
    } catch {
      // The untyped capture refuses the bigint; the trap counts up to that point still matter.
    }
    return counts;
  };
  const typed = traps(echo.params);
  expect(typed).toEqual({ ownKeys: 1, descriptor: 5, get: 0 });
  expect(traps(undefined)).toEqual({ ownKeys: 1, descriptor: 1, get: 0 });
  // A variant proxy is read once too, even though tag is needed before val is typed.
  const variant = { ownKeys: 0, descriptor: 0 };
  const pick = new Proxy(
    { val: 1n, tag: "signed" },
    {
      ownKeys(object) {
        variant.ownKeys++;
        return Reflect.ownKeys(object);
      },
      getOwnPropertyDescriptor(object, key) {
        variant.descriptor++;
        return Reflect.getOwnPropertyDescriptor(object, key);
      },
    },
  );
  expect(text([record(), pick, [0n, ""]])).toContain('{"val":"1","tag":"signed"}');
  expect(variant).toEqual({ ownKeys: 1, descriptor: 2 });
  // toJSON is data like any other field: refused by the record shape, never invoked.
  let called = false;
  const json = record({
    toJSON: () => {
      called = true;
      return {};
    },
  });
  expect(refusal([json, { tag: "none" }, [0n, ""]])).toMatch(/not declared|not portable/);
  expect(called).toBe(false);
  // Class instances, cycles, depth and size bounds are unchanged by typing.
  expect(refusal([Object.assign(Object.create({}), record()), { tag: "none" }, [0n, ""]])).toMatch(
    /plain objects/,
  );
  // In an untyped field of a typed walk, as everywhere else.
  const cycle: unknown[] = [];
  cycle.push(cycle);
  expect(refusal([record({ label: cycle }), { tag: "none" }, [0n, ""]])).toMatch(/cyclic/);
  let deep: unknown = "x";
  for (let level = 0; level < VALUE_DEPTH + 2; level++) deep = [deep];
  expect(refusal([record({ label: deep }), { tag: "none" }, [0n, ""]])).toMatch(/nesting/);
  expect(() => encode([new BigUint64Array(64).fill(MAX), []], 200, wide.params)).toThrow(
    /size bound/,
  );
});

test("lift restores bigint from checked text and refuses non-canonical text", () => {
  const parsed = JSON.parse(
    '[{"id":"18446744073709551615","values":["-1","0"],"maybe":null,"small":7,"label":"123"},{"tag":"signed","val":"-5"},["1","2"]]',
  );
  const before = JSON.stringify(parsed);
  expect(lift(echo.params, parsed)).toEqual([
    { id: MAX, values: [-1n, 0n], maybe: null, small: 7, label: "123" },
    { tag: "signed", val: -5n },
    [1n, "2"],
  ]);
  // The parsed wire data is copied, never updated in place.
  expect(JSON.stringify(parsed)).toBe(before);
  expect(lift(echo.result, { tag: "err", val: "-3" })).toEqual({ tag: "err", val: -3n });
  expect(lift(echo.result, { tag: "ok", val: "0" })).toEqual({ tag: "ok", val: 0n });
  expect(lift(add.result, "1")).toBe(1n);
  expect(lift(plain.result, "123")).toBe("123");
  for (const bad of ["01", 1, "18446744073709551616"])
    expect(() => lift(add.result, bad)).toThrow();
});
