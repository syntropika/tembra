import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createContext, runInContext } from "node:vm";
import { NodeContract } from "@syntropika/tembra-engine-node";

/** The guest's value bootstrap, bundled as the interpreter build bundles it. */
const bootstrap = await (async () => {
  const built = await Bun.build({
    entrypoints: [
      path.resolve(import.meta.dir, "../../../../crates/executor-wasm/guest/src/values.ts"),
    ],
    format: "iife",
    target: "node",
  });
  if (!built.success) throw new AggregateError(built.logs);
  return built.outputs[0]!.text();
})();

/** Load the guest value conversions for one WIT package in a fresh realm. */
async function guestValues(wit: string) {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-values-"),
  );
  try {
    await writeFile(path.join(directory, "contract.wit"), wit);
    const graph = JSON.parse(new NodeContract(directory).request('{"kind":"describe"}')).ok;
    const context = createContext({ __tembraContract: graph });
    runInContext(bootstrap, context);
    return context.__tembraValues as {
      arguments(service: string, method: string, args: unknown[]): unknown[];
      invoke(service: string, method: string, callback: () => unknown): Promise<unknown>;
      importArguments(service: string, method: string, args: unknown[]): unknown[];
      importResult(service: string, method: string, value: unknown): unknown;
      exportArguments(service: string, method: string, args: unknown[]): unknown[];
      exportResult(service: string, method: string, value: unknown): unknown;
    };
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("guest callbacks follow official top-level result exceptions while nested results stay tagged", async () => {
  {
    const values = await guestValues(
      `package example:results;
      interface values {
        type outcome = result<u32, string>;
        record wrapped { value: outcome }
        checked: func() -> outcome;
        nested: func() -> wrapped;
        plain: func() -> u32;
      }
      world test { import values; export values; }`,
    );
    const result = await values.invoke("values", "checked", async () => 42);
    expect(JSON.stringify(result)).toBe('{"tag":"ok","val":42}');
    const rejected = await values.invoke("values", "checked", () => {
      throw Object.assign(new Error("Rejected"), { payload: "denied" });
    });
    expect(JSON.stringify(rejected)).toBe('{"tag":"err","val":"denied"}');
    expect(values.importResult("example:results/values", "checked", result)).toBe(42);
    try {
      values.importResult("example:results/values", "checked", rejected);
      throw new Error("Expected the declared result error");
    } catch (error) {
      expect(error).toBe("denied");
    }
    const nested = { value: { tag: "err", val: "nested" } };
    expect(JSON.stringify(await values.invoke("values", "nested", () => nested))).toBe(
      JSON.stringify(nested),
    );
    expect(JSON.stringify(values.importResult("example:results/values", "nested", nested))).toBe(
      JSON.stringify(nested),
    );
    const trap = new Error("Unexpected failure");
    await expect(
      values.invoke("values", "plain", () => {
        throw trap;
      }),
    ).rejects.toBe(trap);
  }
});

const PRECISION = `package example:precision;
interface numbers {
  type big = u64;
  record rec { id: big, values: list<s64>, maybe: option<u64>, small: u32, label: string }
  variant pick { signed(s64), none }
  add: func(a: u64, b: s64) -> u64;
  echo: func(r: rec, p: pick, t: tuple<u64, string>) -> result<u64, s64>;
  plain: func(x: u32) -> string;
}
world test { import numbers; export numbers; }`;
const MAX = (1n << 64n) - 1n;
const MIN = -(1n << 63n);

test("guest u64/s64: bigint in JavaScript, canonical decimal text on the wire, nothing else", async () => {
  const values = await guestValues(PRECISION);
  const wire = (value: unknown) => JSON.parse(JSON.stringify(value));
  // Exported method arguments lift from the wire; results lower to it.
  const args = values.arguments("numbers", "add", ["18446744073709551615", "-9223372036854775808"]);
  expect(args).toEqual([MAX, MIN]);
  expect(wire(await values.invoke("numbers", "add", () => 0n))).toBe("0");
  expect(wire(await values.invoke("numbers", "add", () => (1n << 53n) + 1n))).toBe(
    "9007199254740993",
  );
  // Nested containers, aliases, typed lists, options, variants, tuples and results.
  const record = {
    id: MAX,
    values: new BigInt64Array([MIN, -1n, 0n, (1n << 63n) - 1n]),
    maybe: undefined,
    small: 7,
    label: "123",
  };
  const lowered = wire(
    values.importArguments("example:precision/numbers", "echo", [
      record,
      { tag: "signed", val: -5n },
      [1n, "2"],
    ]),
  );
  expect(lowered).toEqual([
    {
      id: "18446744073709551615",
      values: ["-9223372036854775808", "-1", "0", "9223372036854775807"],
      maybe: null,
      small: 7,
      label: "123",
    },
    { tag: "signed", val: "-5" },
    ["1", "2"],
  ]);
  const lifted = values.arguments("numbers", "echo", lowered) as [any, any, any];
  expect(lifted[0].id).toBe(MAX);
  expect(Object.prototype.toString.call(lifted[0].values)).toBe("[object BigInt64Array]");
  expect([...lifted[0].values]).toEqual([MIN, -1n, 0n, (1n << 63n) - 1n]);
  expect(lifted[0].maybe).toBeUndefined();
  expect(lifted[0].label).toBe("123");
  expect(lifted[1]).toEqual({ tag: "signed", val: -5n });
  expect(lifted[2]).toEqual([1n, "2"]);
  expect(values.importResult("example:precision/numbers", "echo", { tag: "ok", val: "1" })).toBe(
    1n,
  );
  try {
    values.importResult("example:precision/numbers", "echo", { tag: "err", val: "-2" });
    throw new Error("Expected the declared result error");
  } catch (error) {
    expect(error).toBe(-2n);
  }
  expect(
    wire(
      await values.invoke("numbers", "echo", () => {
        throw Object.assign(new Error("negative"), { payload: -3n });
      }),
    ),
  ).toEqual({ tag: "err", val: "-3" });
  // Wrong kinds and limits are refused in both directions; strings stay strings.
  for (const bad of [1, "1", true, null, undefined, MAX + 1n, -1n, Object(1n)])
    expect(() => values.importArguments("example:precision/numbers", "add", [bad, 0n])).toThrow();
  expect(() =>
    values.importArguments("example:precision/numbers", "add", [0n, 1n << 63n]),
  ).toThrow();
  for (const bad of ["01", "-0", "+1", " 1", "1e3", "18446744073709551616", 1, null])
    expect(() => values.arguments("numbers", "add", [bad, "0"])).toThrow();
  expect(() => values.arguments("numbers", "add", ["0", "-9223372036854775809"])).toThrow();
  expect(() =>
    values.importArguments("example:precision/numbers", "echo", [
      { ...record, values: new Uint32Array([1]) },
      { tag: "none" },
      [1n, "x"],
    ]),
  ).toThrow();
  expect(values.arguments("numbers", "plain", [7])).toEqual([7]);
});
