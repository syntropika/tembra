// The JavaScript host of the typed-state spike. jco-transpile 0.23.0, the SDK's version, turns the
// component into JavaScript; the host keeps the state as the kernel does, as a JSON-safe value,
// and serves the guest's state imports in four ways:
//
// - JSON text: `JSON.parse` and `JSON.stringify`, as `kernel.ts` serves `tembra:guest/state`;
// - the same JSON text in the exact encoding (camelCase, 64-bit integers as strings);
// - a WIT record lifted by jco's generated code, converted to and from the kernel's JSON-safe
//   value (`bigint` as a decimal string) by a converter walking the WIT type, as the SDK's
//   value conversion does;
// - a WIT record kept as jco's object, without conversion: the cost of jco's lifting alone.
//
// The SDK runs exports with JSPI; the spike calls each export once, around the whole loop, so it
// instantiates without JSPI.

import { generate, $init } from "@bytecodealliance/jco-transpile/component";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const component = await readFile(process.argv[2]);
await $init;
const out = path.join(here, "out");
await mkdir(out, { recursive: true });
const { files } = generate(component, {
  name: "orb",
  noTypescript: true,
  instantiation: { tag: "async" },
  noNodejsCompat: true,
  base64Cutoff: 0,
});
for (const [name, bytes] of files) await writeFile(path.join(out, name), bytes);
const { instantiate } = await import(pathToFileURL(path.join(out, "orb.js")).href);
const core = async (name) => WebAssembly.compile(await readFile(path.join(out, name)));

/** The ledger with `entries` entries, as the kernel keeps it. */
function ledger(entries, exact) {
  const big = (1n << 60n) + 7n;
  // JSON today: the number loses precision past 2^53, as it does in tembra's JS hosts.
  const number = (value) => (exact ? value.toString() : Number(value));
  const list = Array.from({ length: entries }, (_, i) => ({
    id: number(big + BigInt(i)),
    owner: `owner-${i}`,
    amount: number(1_000_000n + BigInt(i)),
    tags: ["alpha", "beta"],
  }));
  return exact
    ? { userId: number(big), name: "ledger", balance: "0", entries: list }
    : { user_id: number(big), name: "ledger", balance: 0, entries: list };
}

/** The WIT type of the ledger, as a converter walking the type sees it. */
const ENTRY = {
  record: [
    ["id", "u64"],
    ["owner", "string"],
    ["amount", "u64"],
    ["tags", { list: "string" }],
  ],
};
const LEDGER = {
  record: [
    ["userId", "u64"],
    ["name", "string"],
    ["balance", "s64"],
    ["entries", { list: ENTRY }],
  ],
};

/** A lifted value as the kernel's JSON-safe value. */
function toKernel(type, value) {
  if (type === "u64" || type === "s64") return value.toString();
  if (type === "string") return value;
  if (type.list) return value.map((item) => toKernel(type.list, item));
  const out = {};
  for (const [name, field] of type.record) out[name] = toKernel(field, value[name]);
  return out;
}

/** The kernel's value as the value jco lowers. */
function toGuest(type, value) {
  if (type === "u64" || type === "s64") return BigInt(value);
  if (type === "string") return value;
  if (type.list) return value.map((item) => toGuest(type.list, item));
  const out = {};
  for (const [name, field] of type.record) out[name] = toGuest(field, value[name]);
  return out;
}

async function run(fn, typed, state, iterations) {
  let kept = state;
  const imports = {
    "spike:state/json-state": {
      read: () => JSON.stringify(kept),
      update: (text) => {
        kept = JSON.parse(text);
        return JSON.stringify(kept);
      },
    },
    "spike:state/typed-state":
      typed === "converted"
        ? {
            read: () => toGuest(LEDGER, kept),
            update: (value) => {
              kept = toKernel(LEDGER, value);
              return toGuest(LEDGER, kept);
            },
          }
        : {
            read: () => kept,
            update: (value) => (kept = value),
          },
    "spike:state/types": {},
  };
  const instance = await instantiate(core, imports);
  const bench = instance.bench ?? instance["spike:state/bench"];
  const start = performance.now();
  const sum = bench[fn](iterations);
  const micros = ((performance.now() - start) * 1000) / iterations;
  return { sum: BigInt(sum), micros };
}

console.log("| entries | encoding | µs per read+update |");
console.log("|---:|---|---:|");
for (const [entries, iterations] of [
  [3, 20_000],
  [100, 2_000],
  [1_000, 200],
]) {
  let expected = 0n;
  for (let i = 1n; i <= BigInt(iterations); i++) expected += i;
  for (const [label, fn, exact, typed] of [
    ["JSON today", "runJson", false, null],
    ["JSON exact (camelCase, u64 as string)", "runJsonExact", true, null],
    ["WIT record, jco + kernel conversion", "runTyped", true, "converted"],
    ["WIT record, jco only (reference)", "runTyped", null, "raw"],
  ]) {
    const state = () =>
      typed === "raw" ? toGuest(LEDGER, ledger(entries, true)) : ledger(entries, exact);
    // Warm up, then take the median of five runs, each on a fresh instance.
    await run(fn, typed, state(), 50);
    const runs = [];
    for (let i = 0; i < 5; i++) {
      const { sum, micros } = await run(fn, typed, state(), iterations);
      if (sum !== expected) throw new Error(`${label}: sum ${sum} != ${expected}`);
      runs.push(micros);
    }
    runs.sort((a, b) => a - b);
    console.log(`| ${entries} | ${label} | ${runs[2].toFixed(1)} |`);
  }
}
