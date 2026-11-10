// The real bridge's admission result controls resource transfer in generated guest bindings.
import { expect, test } from "bun:test";
import { createContext, runInContext } from "node:vm";
import path from "node:path";
import { StandardHost } from "../src/interpreter/standard-host.ts";

const imports = ["wasi:http/types@0.3.0", "wasi:http/client@0.3.0"];
const scripts: string[] = [];
for (const entry of ["bridge", "standard"]) {
  const built = await Bun.build({
    entrypoints: [
      path.resolve(import.meta.dir, `../../../../crates/executor-wasm/guest/src/${entry}.ts`),
    ],
    target: "node",
    format: "iife",
  });
  if (!built.success) throw new AggregateError(built.logs);
  scripts.push(await built.outputs[0]!.text());
}

function guest() {
  let active = true;
  const standard = new StandardHost(new Set(imports), [], 8 << 20);
  const globals: any = {
    Uint8Array,
    DataView,
    Symbol,
    __scopeActive: () => active,
    __tembraContract: { imports: imports.map((contract) => ({ contract, type_only: false })) },
    __hostSyncBytes: (packet: Uint8Array) => {
      if (!active) throw new Error("callback scope is closed");
      return standard.sync(packet);
    },
  };
  const context = createContext(globals);
  for (const script of scripts) runInContext(script, context);
  const { Fields, Request } = globals.__tembraStandard[imports[0]!];
  const [request, done] = Request.new(
    new Fields(),
    undefined,
    Promise.resolve({ tag: "ok", val: undefined }),
    undefined,
  );
  return {
    standard,
    globals,
    request,
    done,
    Fields,
    setActive: (value: boolean) => {
      active = value;
    },
    send: globals.__tembraStandard[imports[1]!].send,
    dispose: async () => {
      globals.__tembraStopEffects();
      globals.__tembraSweepStandard(true);
      await standard.dispose();
    },
  };
}

for (const reason of ["inactive", "full"] as const) {
  test(`an owned request remains disposable when async admission is ${reason}`, async () => {
    const engine = guest();
    // Let the input trailers writer finish before the admission fault is introduced.
    await new Promise((resolve) => setTimeout(resolve, 0));
    try {
      if (reason === "inactive") engine.setActive(false);
      else
        for (let count = 0; count < 128; count++)
          engine.globals.__hostAsync(new Uint8Array()).catch(() => {});
      expect(() => engine.send(engine.request)).toThrow(
        reason === "inactive" ? "scope is closed" : "queue is full",
      );
      expect(engine.globals.__tembraTakeEffects().length).toBe(reason === "inactive" ? 0 : 128);
      engine.setActive(true);
      engine.globals.__tembraStopEffects();
      engine.request[Symbol.dispose]();
      engine.done[Symbol.dispose]();
      expect(engine.standard.size).toBe(0);
    } finally {
      engine.setActive(true);
      await engine.dispose();
    }
  });
}

test("an output future remains owned if the bridge refuses its first read", async () => {
  const engine = guest();
  await new Promise((resolve) => setTimeout(resolve, 0));
  try {
    engine.setActive(false);
    await expect(Promise.resolve(engine.done)).rejects.toThrow("scope is closed");
    engine.setActive(true);
    engine.request[Symbol.dispose]();
    engine.done[Symbol.dispose]();
    expect(engine.standard.size).toBe(0);
  } finally {
    engine.setActive(true);
    await engine.dispose();
  }
});

test("a host refusal after enqueue releases an untaken owned request", async () => {
  const engine = guest();
  await new Promise((resolve) => setTimeout(resolve, 0));
  const retained: Disposable[] = [];
  try {
    while (engine.standard.size < 4096) retained.push(new engine.Fields());
    const pending = engine.send(engine.request);
    const [effect] = engine.globals.__tembraTakeEffects();
    expect(effect).toBeDefined();
    try {
      await engine.standard.async(effect.packet);
      throw new Error("The full handle table should refuse a response slot");
    } catch (error) {
      engine.globals.__tembraCompleteEffect(effect.id, false, String(error));
    }
    await expect(pending).rejects.toThrow("handle table is full");
    engine.done[Symbol.dispose]();
    for (const fields of retained) fields[Symbol.dispose]();
    expect(engine.standard.size).toBe(0);
  } finally {
    await engine.dispose();
  }
});

test("a refused future read calls explicit then and catch rejection handlers", async () => {
  const engine = guest();
  await new Promise((resolve) => setTimeout(resolve, 0));
  try {
    engine.setActive(false);
    const errors: string[] = [];
    await engine.done.then(undefined, (error: Error) => errors.push(error.message));
    await engine.done.catch((error: Error) => errors.push(error.message));
    expect(errors).toEqual(["callback scope is closed", "callback scope is closed"]);
    engine.setActive(true);
    engine.request[Symbol.dispose]();
    engine.done[Symbol.dispose]();
    expect(engine.standard.size).toBe(0);
  } finally {
    engine.setActive(true);
    await engine.dispose();
  }
});
