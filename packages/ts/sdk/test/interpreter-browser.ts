import {
  createHost,
  memoryFilesystem,
  opfsFilesystem,
  type FilesystemProvider,
} from "../src/browser.ts";
import type { OrbArtifact } from "../src/authoring/types.ts";

/** Qualify actual Wasm callbacks, isolated descriptors, OPFS persistence and bounded termination. */
export async function verifyBrowserInterpreter(): Promise<void> {
  const source = async (name: string) => {
    const response = await fetch(`/${name}.wasm`);
    if (!response.ok) throw new Error(`Missing fixture ${name}`);
    return new Uint8Array(await response.arrayBuffer());
  };
  const memory = memoryFilesystem({ "input.txt": "initial" });
  const root = await navigator.storage.getDirectory();
  const directoryName = `tembra-${crypto.randomUUID()}`;
  const directory = await root.getDirectoryHandle(directoryName, { create: true });
  const nested = await directory.getDirectoryHandle("nested", { create: true });
  const seeded = await (await nested.getFileHandle("input.txt", { create: true })).createWritable();
  await seeded.write("seeded");
  await seeded.close();
  const durable = await opfsFilesystem(directory);
  const messages: string[] = [];
  let mounts = 0;
  const tracked = (provider: FilesystemProvider): FilesystemProvider => ({
    open(mount, writable) {
      const instance = provider.open(mount, writable);
      mounts++;
      return {
        ...instance,
        async dispose() {
          await instance.dispose();
          mounts--;
        },
      };
    },
  });
  const host = await createHost({
    isolated: {
      engine: new URL("/engine/interpreter.js", location.href),
      providers: { filesystem: { memory: tracked(memory), durable: tracked(durable) } },
      limits: { timeoutMs: 1000 },
      onLog: (record) => {
        messages.push(record.message);
      },
    },
  });
  const requires = { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } };
  const calculator: OrbArtifact = { artifact: await source("calculator"), requires };
  const storage = async (
    name: string,
    provider: string,
    writable: boolean,
  ): Promise<OrbArtifact> => ({
    artifact: await source(name),
    grants: { filesystem: [{ provider, mount: "/data", writable }] },
  });
  const equal = (actual: unknown, expected: unknown) => {
    if (actual !== expected) throw new Error(`Expected ${expected}, received ${actual}`);
  };
  const rejects = async (operation: Promise<unknown>): Promise<string> => {
    try {
      await operation;
    } catch (error) {
      return String(error);
    }
    throw new Error("Forbidden operation succeeded");
  };
  let primary: unknown;
  let stage = "install";
  let wrote = false;
  try {
    await host.install([
      { artifact: await source("arithmetic-source") },
      calculator,
      await storage("writer", "memory", true),
      await storage("reader", "memory", false),
      await storage("persistent", "durable", true),
      await storage("persistent-reader", "durable", false),
    ]);
    stage = "calculator";
    const calculation = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
    equal(await calculation.sum({ values: [2, 4, 6] }), 24);
    equal(calculation.state.value, 24);
    equal(await calculation["resource-value"](), 42);
    await rejects(calculation.fail());
    await rejects(calculation.forbidden());
    await rejects(calculation.malformed());
    equal(await calculation.recover(), 42);
    equal(await calculation.fanout(), 24);
    equal(JSON.stringify(await calculation.checked(7)), JSON.stringify({ tag: "ok", val: 7 }));
    equal(
      JSON.stringify(await calculation.checked(-1)),
      JSON.stringify({ tag: "err", val: "negative" }),
    );
    stage = "filesystem";
    const writer = await host.get("writer::example:storage/files@1.0.0", "main");
    const reader = await host.get("reader::example:storage/files@1.0.0", "main");
    const persistent = await host.get("persistent::example:storage/files@1.0.0", "main");
    equal(await reader.read("/data/input.txt"), "initial");
    await writer.write("/data/input.txt", "changed");
    equal(await reader.read("/data/input.txt"), "changed");
    await rejects(reader.write("/data/input.txt", "forbidden"));
    await rejects(reader.read("/data/../outside.txt"));
    equal(await writer.read("/data/input.txt"), "changed");
    stage = "opfs";
    await persistent.write("/data/output.txt", "persisted");
    wrote = true;
    equal(await persistent.read("/data/output.txt"), "persisted");
    const durableReader = await host.get("persistent-reader::example:storage/files@1.0.0", "main");
    equal(await durableReader.read("/data/nested/input.txt"), "seeded");
    await rejects(durableReader.write("/data/nested/input.txt", "forbidden"));
    equal(await persistent.read("/data/nested/input.txt"), "seeded");
    durableReader.release();
    stage = "concurrent effects";
    equal(await calculation.race(), -1);
    equal(messages.includes("slow-finished"), false);
    equal(await calculation.recover(), 42);
    stage = "reload";
    const candidate = await host.prepare([calculator]);
    await candidate.abort();
    await host.reload([calculator]);
    equal(await calculation.read(), 24);
    equal(await calculation["resource-value"](), 42);
    stage = "cancellation";
    await rejects(
      host.root(async (context) => {
        const pending = calculation.pending(context);
        setTimeout(() => context.cancel(), 5);
        await pending;
      }),
    );
    equal(await calculation.recover(), 42);
    stage = "termination";
    for (const [name, input] of [
      ["cpu", 1],
      ["suspended", 2],
    ] as const) {
      await host.install({ artifact: await source(`${name}-source`) });
      await rejects(host.get(`${name}::example:integrated/arithmetic@1.0.0`, "broken"));
      const reference = await host.get(`${name}::example:integrated/arithmetic@1.0.0`, "main");
      equal(await reference.double(3), 6);
      await rejects(reference.double(5));
      equal(await reference.double(3), 6);
      equal(await reference.double(4), 42);
      equal(await reference.double(3), 6);
      await rejects(reference.double(input));
      reference.release();
    }
    calculation.release();
    writer.release();
    reader.release();
    persistent.release();
  } catch (error) {
    primary = `${stage}: ${error instanceof Error ? error.stack : error}`;
  } finally {
    try {
      await host.close();
    } catch (error) {
      primary = `${primary ?? ""}; cleanup: ${String(error)}`;
    } finally {
      try {
        if (wrote)
          equal(
            await (await (await directory.getFileHandle("output.txt")).getFile()).text(),
            "persisted",
          );
      } finally {
        await root.removeEntry(directoryName, { recursive: true });
      }
    }
  }
  if (primary) throw new Error(String(primary));
  equal(messages.includes("slow-finished"), true);
  equal(mounts, 0);
  equal(host.diagnostics().values, 0);
}
