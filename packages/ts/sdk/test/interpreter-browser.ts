import { createHost, createInterpreter, memoryFilesystem, opfsFilesystem } from "../src/browser.ts";
import type { InterpreterOrbOptions, PreparedSource } from "../src/interpreter/types.ts";

/** Exercise actual managed callbacks in a browser, including descriptor isolation and OPFS flushing. */
export async function verifyBrowserInterpreter(): Promise<void> {
  const source = (await fetch("/orb.json").then((response) => response.json())) as PreparedSource;
  const files = (await fetch("/files.json").then((response) => response.json())) as PreparedSource;
  const memory = memoryFilesystem({ "input.txt": "initial" });
  const root = await navigator.storage.getDirectory();
  const directoryName = `tembra-${crypto.randomUUID()}`;
  const directory = await root.getDirectoryHandle(directoryName, { create: true });
  const durable = await opfsFilesystem(directory);
  const engine = await createInterpreter({
    engine: new URL("/engine/interpreter.js", location.href),
    providers: { filesystem: { memory, durable } },
    limits: { timeoutMs: 1000 },
  });
  const host = await createHost();
  const sourceOptions: InterpreterOrbOptions = {
    name: "calculator",
    apiVersion: 1,
    source,
    bindings: {
      "example:integrated/arithmetic@1.0.0": {
        kind: "orb",
        name: "arithmetic",
        api_version: 1,
        world: "arithmetic-provider",
      },
    },
  };
  const storage = (name: string, provider: string, writable: boolean): InterpreterOrbOptions => ({
    name,
    apiVersion: 1,
    source: files,
    bindings: {
      "wasi:filesystem/types@0.3.0": { kind: "host", provider },
      "wasi:filesystem/preopens@0.3.0": { kind: "host", provider },
      "wasi:clocks/system-clock@0.3.0": { kind: "host", provider: "system-clock" },
    },
    grants: { filesystem: [{ provider, mount: "/data", writable }] },
  });
  const equal = (actual: unknown, expected: unknown) => {
    if (actual !== expected) throw new Error(`Expected ${expected}, received ${actual}`);
  };
  const rejects = async (operation: Promise<unknown>) => {
    let refused = false;
    try {
      await operation;
    } catch (error) {
      refused = true;
      return String(error);
    }
    if (!refused) throw new Error("Forbidden operation succeeded");
  };
  let primary: unknown;
  let stage = "install";
  let wrote = false;
  try {
    await host.install([
      {
        name: "arithmetic",
        apiVersion: 1,
        services: {
          "arithmetic::example:integrated/arithmetic@1.0.0": {
            methods: { double: (_e, _c, value: number) => value * 2 },
          },
        },
      },
      engine.orb(sourceOptions),
      engine.orb(storage("writer", "memory", true)),
      engine.orb(storage("reader", "memory", false)),
      engine.orb(storage("persistent", "durable", true)),
    ]);
    stage = "calculator";
    const calculator = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
    equal(await calculator.sum({ values: [2, 4, 6] }), 24);
    equal(calculator.state.value, 24);
    equal(await calculator["resource-value"](), 42);
    await rejects(calculator.fail());
    equal(await calculator.recover(), 42);
    await rejects(calculator.forbidden());
    stage = "filesystem";
    const writer = await host.get("writer::example:storage/files@1.0.0", "main");
    const reader = await host.get("reader::example:storage/files@1.0.0", "main");
    const persistent = await host.get("persistent::example:storage/files@1.0.0", "main");
    equal(await reader.read("/data/input.txt"), "initial");
    await writer.write("/data/input.txt", "changed");
    stage = "read-after-write";
    equal(await reader.read("/data/input.txt"), "changed");
    await rejects(reader.write("/data/input.txt", "forbidden"));
    await rejects(reader.read("/data/../outside.txt"));
    stage = "read-after-denial";
    equal(await writer.read("/data/input.txt"), "changed");
    stage = "opfs";
    await persistent.write("/data/output.txt", "persisted");
    wrote = true;
    stage = "reload";
    const candidate = await host.prepare([engine.orb(sourceOptions)]);
    await candidate.abort();
    await host.reload([engine.orb(sourceOptions)]);
    equal(await calculator.read(), 24);
    equal(await calculator["resource-value"](), 42);
    // Cancelling a managed wait retains the task until its imported operation physically settles.
    stage = "cancellation";
    await rejects(
      host.root(async (ctx) => {
        const pending = calculator.pending(ctx);
        setTimeout(() => ctx.cancel(), 5);
        await pending;
      }),
    );
    equal(await calculator.recover(), 42);
    stage = "guest-termination";
    const guard = (await fetch("/guard.json").then((response) =>
      response.json(),
    )) as PreparedSource;
    for (const [name, input] of [
      ["cpu", 1],
      ["suspended", 2],
    ] as const) {
      await host.install(engine.orb({ name, apiVersion: 1, source: guard }));
      await rejects(host.get(`${name}::example:integrated/arithmetic@1.0.0`, "broken"));
      const reference = await host.get(`${name}::example:integrated/arithmetic@1.0.0`, "main");
      equal(await reference.double(3), 6);
      await rejects(reference.double(5));
      equal(await reference.double(3), 6);
      equal(await reference.double(4), 42);
      equal(await reference.double(3), 6);
      console.log("Guest termination", name, await rejects(reference.double(input)));
      reference.release();
    }
    calculator.release();
    writer.release();
    reader.release();
    persistent.release();
  } catch (error) {
    primary = `${stage}: ${error instanceof Error ? error.stack : error}`;
  } finally {
    try {
      await host.close();
    } catch (error) {
      const describe = (error: unknown): string =>
        error instanceof AggregateError ? error.errors.map(describe).join("; ") : String(error);
      primary = `${primary ?? ""}; cleanup: ${describe(error)}`;
    } finally {
      try {
        if (wrote)
          equal(
            await (
              await directory.getFileHandle("output.txt")
            )
              .getFile()
              .then((file) => file.text()),
            "persisted",
          );
      } finally {
        await root.removeEntry(directoryName, { recursive: true });
      }
    }
  }
  if (primary) throw new Error(String(primary));
  equal(engine.instances(), 0);
  equal(host.diagnostics().values, 0);
}
