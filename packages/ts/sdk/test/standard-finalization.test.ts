import { expect, test } from "bun:test";
import { runInNewContext } from "node:vm";
import path from "node:path";
import {
  CALL,
  DROP,
  Reader,
  Writer,
  encode,
  type HandleCodec,
} from "../src/interpreter/standard-codec.ts";
import { METHODS } from "../src/interpreter/standard.ts";

const filesystem = "wasi:filesystem/preopens@0.3.0";
const types = "wasi:filesystem/types@0.3.0";
const method = (name: string) => METHODS.findIndex((entry) => entry.name === name);
const raw: HandleCodec = { handle: (_k, _d, value) => value as number, value: (_k, _d, id) => id };
const built = await Bun.build({
  entrypoints: [
    path.resolve(import.meta.dir, "../../../../crates/executor-wasm/guest/src/standard.ts"),
  ],
  target: "node",
  format: "iife",
});
if (!built.success) throw new AggregateError(built.logs);
const script = await built.outputs[0]!.text();

/** Control collection notification without relying on a GC timing heuristic. */
function environment() {
  let notify!: (owner: unknown) => void;
  const registrations = new Map<object, { owner: unknown; token: object }>();
  const dropped: number[] = [];
  let settle!: (bytes: Uint8Array) => void;
  let next = 0;
  const reply = (name: string, value: unknown) => {
    const writer = new Writer();
    encode(writer, METHODS[method(name)]!.result!, value, raw);
    return writer.finish();
  };
  const globals: any = {
    Uint8Array,
    Symbol,
    __tembraContract: {
      imports: [
        { contract: filesystem, type_only: false },
        { contract: types, type_only: false },
      ],
    },
    FinalizationRegistry: class {
      constructor(callback: (owner: unknown) => void) {
        notify = callback;
      }
      register(target: object, owner: unknown, token: object) {
        registrations.set(target, { owner, token });
      }
      unregister(token: object) {
        for (const [target, entry] of registrations)
          if (entry.token === token) registrations.delete(target);
      }
    },
    __hostSyncBytes(packet: Uint8Array) {
      const reader = new Reader(packet);
      reader.u8();
      if (reader.u8() === DROP) {
        dropped.push(reader.handle());
        return new Uint8Array();
      }
      expect(new Reader(packet.subarray(1)).u8()).toBe(CALL);
      expect(reader.u32()).toBe(method("getDirectories"));
      return reply("getDirectories", [[++next, "/data"]]);
    },
    __hostAsync: () =>
      new Promise<Uint8Array>((resolve) => {
        settle = resolve;
      }),
  };
  runInNewContext(script, globals);
  return {
    directory: () => globals.__tembraStandard[filesystem].getDirectories()[0][0],
    sweep: () => globals.__tembraSweepStandard(),
    collect(target: object) {
      const entry = registrations.get(target);
      if (entry) {
        registrations.delete(target);
        notify(entry.owner);
      }
    },
    completeStat: () =>
      settle(
        reply("stat", {
          tag: "ok",
          val: {
            type: { tag: "directory" },
            linkCount: 1n,
            size: 0n,
          },
        }),
      ),
    dropped,
  };
}

test("collection waits for an outstanding borrow and drops only at a safe boundary", async () => {
  const env = environment();
  const directory = env.directory();
  const pending = directory.stat();
  env.collect(directory);
  expect(env.dropped).toEqual([]);
  env.sweep();
  expect(env.dropped).toEqual([]);
  env.completeStat();
  await pending;
  env.sweep();
  expect(env.dropped).toEqual([1]);
  env.sweep();
  expect(env.dropped).toEqual([1]);
});

test("explicit drop unregisters finalization and cannot double-close the resource", () => {
  const env = environment();
  const directory = env.directory();
  directory[Symbol.dispose]();
  env.collect(directory);
  env.sweep();
  expect(env.dropped).toEqual([1]);
  expect(() => directory[Symbol.dispose]()).toThrow("already dropped");
});
