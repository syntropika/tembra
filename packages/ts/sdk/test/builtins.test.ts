import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { createContext, runInContext } from "node:vm";

const source = new Bun.Transpiler({ loader: "ts" }).transformSync(
  readFileSync(
    new URL("../../../../crates/executor-wasm/guest/src/builtins.ts", import.meta.url),
    "utf8",
  ),
);

function guest(clock: boolean, seed: [string, string] | null) {
  const context = createContext({ __tembraBuiltins: { clock, seed } });
  runInContext("Date.now = () => 1234; globalThis.performance = {now: () => 42};", context);
  runInContext(source, context);
  return (expression: string): any => runInContext(expression, context);
}

test("source builtins refuse ambient time and entropy, including Date constructor paths", () => {
  const evaluate = guest(false, null);
  for (const expression of [
    "Date.now()",
    "Date()",
    "Date(0)",
    "new Date()",
    "new (new Date(0).constructor)()",
    "Date.prototype.constructor.now()",
    "new (class extends Date {})()",
    "performance.now()",
  ])
    expect(() => evaluate(expression)).toThrow("clock capability is not granted");
  expect(() => evaluate("Math.random()")).toThrow("random capability is not granted");
  expect(evaluate("new Date(0).toISOString()")).toBe("1970-01-01T00:00:00.000Z");
  expect(evaluate("Date.parse('1970-01-01T00:00:00Z')")).toBe(0);
  expect(evaluate("Date.UTC(2000, 0, 1)")).toBe(946684800000);
  expect(evaluate("new (class extends Date {})(1234).getTime()")).toBe(1234);
  expect(evaluate("typeof __tembraBuiltins")).toBe("undefined");
  evaluate("globalThis.__tembraBuiltins = {clock:true,seed:['1','2']}");
  expect(() => evaluate("Date.now()")).toThrow("clock capability is not granted");
  expect(() => evaluate("Math.random()")).toThrow("random capability is not granted");
});

test("granted time uses the native clock and insecure random keeps deterministic heap state", () => {
  const first = guest(true, ["18446744073709551615", "9007199254740993"]);
  const second = guest(false, ["18446744073709551615", "9007199254740993"]);
  expect(first("Date.now()")).toBe(1234);
  expect(first("performance.now()")).toBe(42);
  const sequence = "Array.from({length:20}, () => Math.random())";
  const values = first(sequence) as number[];
  expect([...values]).toEqual([...second(sequence)]);
  expect(values.every((value) => value >= 0 && value < 1)).toBe(true);
  expect(new Set(values).size).toBe(20);
  expect([...first(sequence)]).not.toEqual([...values]);
  const zero = guest(false, ["0", "0"]);
  expect(new Set(zero(sequence)).size).toBeGreaterThan(1);
});
