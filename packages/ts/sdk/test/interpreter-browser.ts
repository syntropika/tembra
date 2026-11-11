import {
  createHost,
  FilesystemError,
  fetchHttpProvider,
  memoryFilesystem,
  opfsFilesystem,
  type FilesystemProvider,
  type Descriptor,
  type DirectoryDescriptor,
  type FileDescriptor,
} from "../src/browser.ts";
import type { OrbArtifact } from "../src/authoring/types.ts";
import { verifyHttpScenario, verifyRawHttpScenario } from "./http-browser.ts";
import { verifySocketScenario } from "./sockets-browser.ts";
// Incoming HTTP acceptance is invoked by the browser qualification page.
export { verifyIngressScenario } from "./ingress-browser.ts";
import { verifyPrecisionScenario } from "./precision-scenario.ts";
import { verifyContextAcquireScenario } from "./context-acquire-scenario.ts";

/** Chrome entry of the same keyed-acquisition scenario used by the packaged Node consumer. */
export async function verifyContextAcquireBrowser(): Promise<string> {
  const host = await createHost({});
  try {
    return await verifyContextAcquireScenario(host, source);
  } finally {
    await host.close();
  }
}

/** Chrome entry of the exact u64/s64 scenario; called once the 64-bit fixtures are built. */
export async function verifyPrecisionBrowser(): Promise<string> {
  const host = await createHost({});
  try {
    return await verifyPrecisionScenario(host, source);
  } finally {
    await host.close();
  }
}

/** Fetch one prepared qualification artifact from the test server. */
async function source(name: string): Promise<Uint8Array> {
  const response = await fetch(`/${name}.wasm`);
  if (!response.ok) throw new Error(`Missing fixture ${name}`);
  return new Uint8Array(await response.arrayBuffer());
}

/** Install two source components through one shared runtime and make independent typed calls. */
export async function verifyArithmeticSource(): Promise<void> {
  const host = await createHost({});
  try {
    await host.install([
      { artifact: await source("arithmetic-source") },
      { artifact: await source("cpu-source") },
    ]);
    const reference = await host.get("arithmetic::example:integrated/arithmetic@1.0.0", "main");
    const doubled = await reference.double(3);
    if (doubled !== 6) throw new Error(`Engine selection returned ${doubled}`);
    reference.release();
    const other = await host.get("cpu::example:integrated/arithmetic@1.0.0", "main");
    if ((await other.double(3)) !== 6)
      throw new Error("The second source component did not answer");
    other.release();
  } finally {
    await host.close();
  }
  if (host.diagnostics().values !== 0) throw new Error("Engine selection leaked values");
}

/** Qualify actual Wasm callbacks, isolated descriptors, OPFS persistence and bounded termination. */
export async function verifyBrowserInterpreter(): Promise<void> {
  await verifySetupRetry();
  const memory = memoryFilesystem({ "input.txt": "initial" });
  const root = await navigator.storage.getDirectory();
  const directoryName = `tembra-${crypto.randomUUID()}`;
  const directory = await root.getDirectoryHandle(directoryName, {
    create: true,
  });
  const nested = await directory.getDirectoryHandle("nested", { create: true });
  const seeded = await (await nested.getFileHandle("input.txt", { create: true })).createWritable();
  await seeded.write("seeded");
  await seeded.close();
  const durable = await opfsFilesystem(directory);
  const messages: string[] = [];
  let requests = 0;
  let responsesClosed = 0;
  let mounts = 0;
  const tracked = (provider: FilesystemProvider): FilesystemProvider => ({
    open(mount, writable) {
      const instance = provider.open(mount, writable);
      mounts++;
      return {
        writable: instance.writable,
        openRoot: () => instance.openRoot(),
        async dispose() {
          await instance.dispose();
          mounts--;
        },
      };
    },
  });
  const host = await createHost({
    isolated: {
      providers: {
        filesystem: { memory: tracked(memory), durable: tracked(durable) },
        http: {
          mock: fetchHttpProvider(async (input, init) => {
            requests++;
            if (new URL(String(input)).origin === location.origin)
              return globalThis.fetch(input, init);
            if (new URL(String(input)).pathname === "/values")
              return Response.json({ values: [2, 4, 6] });
            return new Response(
              new ReadableStream<Uint8Array>({
                start(controller) {
                  controller.enqueue(new TextEncoder().encode("data: streamed before EOF\n\n"));
                },
                cancel() {
                  responsesClosed++;
                },
              }),
              { headers: { "content-type": "text/event-stream" } },
            );
          }),
        },
      },
      limits: { timeoutMs: 1000 },
      onLog: (record) => {
        messages.push(record.message);
      },
    },
  });
  const requires = {
    "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 },
  };
  const calculator: OrbArtifact = {
    artifact: await source("calculator"),
    requires,
  };
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
      {
        artifact: await source("online"),
        requires,
        grants: {
          http: [
            {
              provider: "mock",
              origins: ["https://api.example.test", location.origin],
            },
          ],
        },
      },
      await storage("writer", "memory", true),
      await storage("reader", "memory", false),
      await storage("persistent", "durable", true),
      await storage("persistent-reader", "durable", false),
      { artifact: await source("hook-provider") },
      {
        artifact: await source("hook-extension"),
        requires: Object.fromEntries(
          ["guard", "audit", "choice"].map((hook) => [
            `example:hooks/${hook}@1.0.0`,
            { orb: "tools", apiVersion: 1 },
          ]),
        ),
      },
    ]);
    // The browser is source-only: one source participant decides each portable hook.
    stage = "portable hooks";
    const hooks = await host.get("tools::example:hooks/runner@1.0.0", "main");
    const reporter = await host.get("extension::example:hooks/reporter@1.0.0", "main");
    try {
      equal(
        JSON.stringify(await hooks.run({ value: 3, blocked: false })),
        JSON.stringify({ value: 5, blocked: true }),
      );
      for (const input of [5, 7]) equal(await hooks.audit(input), null);
      equal(
        JSON.stringify(await Promise.all([0, 1, 2].map((input) => hooks.choose(input)))),
        JSON.stringify([null, 11, 84]),
      );
      equal(JSON.stringify(await reporter.read()), JSON.stringify({ audit: 12, choices: 3 }));
    } finally {
      hooks.release();
      reporter.release();
    }
    stage = "calculator";
    const calculation = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
    equal(await calculation.sum({ values: [2, 4, 6] }), 24);
    equal(calculation.state.value, 24);
    equal(await calculation["resource-value"](), 42);
    const installedVersion = host.diagnostics().codeVersion;
    equal((await rejects(host.install(calculator))).includes("Use reload"), true);
    equal(host.diagnostics().codeVersion, installedVersion);
    equal(calculation.state.value, 24);
    // Breaking semantics: a non-`result` export that throws traps and quarantines its revision,
    // so each failing method is proven on its own installation below.
    equal(await calculation.recover(), 42);
    equal(await calculation.fanout(), 24);
    stage = "WASI HTTP provider";
    const online = await host.get("online::example:integrated/network@1.0.0", "main");
    try {
      equal(await online["fetch-and-sum"]("https://api.example.test/values"), 24);
      equal(await online["first-event"]("https://api.example.test/events"), "streamed before EOF");
      equal(responsesClosed, 1);
      equal(requests, 2);
      equal(await online["fetch-and-sum"](`${location.origin}/wasi/values`), 24);
      equal(await online["first-event"](`${location.origin}/wasi/events`), "browser fetch");
      equal(requests, 4);
      await rejects(online["fetch-and-sum"]("https://ungranted.example.test/values"));
      equal(requests, 4);
    } finally {
      online.release();
    }
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
    // `double` is a synchronous WIT import (COORDINATION #6): it completes inside the guest's
    // turn before `Promise.race` starts, so the dependency's value wins.
    equal(await calculation.race(), 12);
    // Returning the race's value does not release the callback before admitted work settles.
    equal(messages.includes("slow-finished"), true);
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
    // Neither JCO nor the runtime can cancel a guest task: the cancelled host work fails the
    // export, which quarantines the revision. Later calls are refused.
    equal(String(await rejects(calculation.recover())).includes("quarantined"), true);
    stage = "termination";
    await host.install({ artifact: await source("suspended-source") });
    await rejects(host.get("suspended::example:integrated/arithmetic@1.0.0", "broken"));
    {
      const reference = await host.get("suspended::example:integrated/arithmetic@1.0.0", "main");
      equal(await reference.double(3), 6);
      // The detached wait stays with this callback until its task really exits; the next call
      // is admitted only afterwards.
      equal(await reference.double(4), 42);
      equal(await reference.double(3), 6);
      // A throw from a non-`result` export traps and quarantines the revision.
      await rejects(reference.double(5));
      equal(String(await rejects(reference.double(3))).includes("quarantined"), true);
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
  for (const method of ["fail", "forbidden", "malformed"]) await trapsAndQuarantines(method);
  equal(messages.includes("slow-finished"), true);
  equal(mounts, 0);
  equal(host.diagnostics().values, 0);
}

/**
 * The real async B1 export returns before its detached host wait ends. That physical work must
 * remain inside the callback deadline; expiry quarantines the revision before another call.
 */
export async function verifyDetachedWorkRefusal(): Promise<void> {
  const host = await createHost({ isolated: { limits: { timeoutMs: 1500 } } });
  try {
    await host.install({ artifact: await source("b1") });
    const reference = (await host.get("b1::tembra-test:b1/probe@1.0.0", "main")) as any;
    try {
      let refused = false;
      try {
        await reference.detach(5000);
      } catch {
        refused = true;
      }
      if (!refused) throw new Error("Detached work outlived its callback deadline");
      let quarantined = false;
      try {
        await reference.observed();
      } catch (error) {
        quarantined = /quarantined/.test(String(error));
      }
      if (!quarantined) throw new Error("A timed-out revision accepted another call");
    } finally {
      reference.release();
    }
  } finally {
    await host.close();
  }
}

/**
 * Fuel bounds a synchronous source loop: `while (true) {}` ends with a fuel trap well before the
 * callback deadline, the revision is quarantined and an independent revision still works.
 */
export async function verifySourceCpuLimit(): Promise<string> {
  const timeoutMs = 20000;
  const host = await createHost({
    isolated: { limits: { timeoutMs, fuel: 50_000_000 } },
  });
  try {
    await host.install({ artifact: await source("cpu-source") });
    const reference = (await host.get("cpu::example:integrated/arithmetic@1.0.0", "main")) as any;
    try {
      if ((await reference.double(3)) !== 6) throw new Error("cpu-source does not double");
      const started = performance.now();
      let trapped = false;
      try {
        await reference.double(1);
      } catch {
        trapped = true;
      }
      const elapsed = performance.now() - started;
      if (!trapped) throw new Error("An endless source loop returned");
      if (elapsed >= timeoutMs) throw new Error("Fuel did not end the loop before the deadline");
      let quarantined = false;
      try {
        await reference.double(3);
      } catch (error) {
        quarantined = /quarantined/.test(String(error));
      }
      if (!quarantined) throw new Error("A fuel trap did not quarantine the revision");
      return `source loop trapped by fuel in ${Math.round(elapsed)} ms`;
    } finally {
      reference.release();
    }
  } finally {
    await host.close();
  }
}

/** A compiled component's loop and memory growth are bounded by the same instance limits. */
export async function verifyCompiledLimits(): Promise<string> {
  const timeoutMs = 20000;
  const host = await createHost({
    isolated: {
      limits: { timeoutMs, fuel: 50_000_000, memoryBytes: 8 << 20 },
      providers: { filesystem: { memory: memoryFilesystem() } },
    },
  });
  try {
    await host.install({
      artifact: await source("compiled-probe"),
      grants: { filesystem: [{ provider: "memory", mount: "/data", writable: false }] },
    });
    const api = (await host.get("probe::example:probe/probe@1.0.0", "main")) as any;
    try {
      if ((await api.spin(0)) !== 0) throw new Error("Compiled probe does not answer");
      // `grow` accepts 64 KiB pages and returns the old size or -1 (as u32) on refusal.
      if ((await api.grow(16)) === 0xffffffff) throw new Error("A grow within budget was refused");
      if ((await api.grow(1024)) !== 0xffffffff) throw new Error("A grow over budget was accepted");
      const started = performance.now();
      let trapped = false;
      try {
        await api.spin(0xffffffff);
      } catch {
        trapped = true;
      }
      const elapsed = performance.now() - started;
      if (!trapped || elapsed >= timeoutMs) throw new Error("Fuel did not end the compiled loop");
      let quarantined = false;
      try {
        await api.spin(0);
      } catch (error) {
        quarantined = /quarantined/.test(String(error));
      }
      if (!quarantined) throw new Error("A compiled fuel trap did not quarantine");
      return `compiled loop trapped by fuel in ${Math.round(elapsed)} ms; grow over 8 MiB refused`;
    } finally {
      api.release();
    }
  } finally {
    await host.close();
  }
}

/** One calculator installation whose failing non-`result` method traps and quarantines it. */
async function trapsAndQuarantines(method: string): Promise<void> {
  const host = await createHost();
  try {
    await host.install([
      { artifact: await source("arithmetic-source") },
      {
        artifact: await source("calculator"),
        requires: {
          "example:integrated/arithmetic@1.0.0": {
            orb: "arithmetic",
            apiVersion: 1,
          },
        },
      },
    ]);
    const calculation = (await host.get(
      "calculator::example:integrated/calculation@1.0.0",
      "main",
    )) as any;
    try {
      let trapped = false;
      try {
        await calculation[method]();
      } catch {
        trapped = true;
      }
      if (!trapped) throw new Error(`${method} succeeded`);
      let quarantined = false;
      try {
        await calculation.recover();
      } catch (error) {
        quarantined = /quarantined/.test(String(error));
      }
      if (!quarantined) throw new Error(`${method}: a trapped revision accepted another call`);
    } finally {
      calculation.release();
    }
  } finally {
    await host.close();
  }
}

/** A transient validator download failure must not poison later installations on the same host. */
async function verifySetupRetry(): Promise<void> {
  const artifact = await source("arithmetic-source");
  const host = await createHost({});
  const fetch = globalThis.fetch;
  let refused = false;
  globalThis.fetch = Object.assign(
    (input: Parameters<typeof fetch>[0], init?: Parameters<typeof fetch>[1]) => {
      const url = input instanceof Request ? input.url : String(input);
      if (!refused && url.endsWith("/contracts_bg.wasm")) {
        refused = true;
        return Promise.reject(new Error("Transient validator download failure"));
      }
      return fetch(input, init);
    },
    fetch,
  );
  try {
    let failed = false;
    try {
      await host.install({ artifact });
    } catch (error) {
      if (!String(error).includes("Transient validator download failure")) throw error;
      failed = true;
    }
    if (!refused || !failed) throw new Error("Validator download failure was not exercised");
    globalThis.fetch = fetch;
    await host.install({ artifact });
    const reference = await host.get("arithmetic::example:integrated/arithmetic@1.0.0", "main");
    try {
      if ((await reference.double(3)) !== 6) throw new Error("Backend retry failed");
    } finally {
      reference.release();
    }
  } finally {
    globalThis.fetch = fetch;
    await host.close();
  }
}

/** Count real opened files while preserving descriptor methods and provider identity. */
function counted(provider: FilesystemProvider) {
  const state = { opened: 0, live: 0 };
  const originals = new WeakMap<Descriptor, Descriptor>();
  function wrap<T extends Descriptor>(descriptor: T, audits: Set<() => Promise<void>>): T {
    let closed = false;
    const markClosed = () => {
      if (closed) return;
      closed = true;
      if (descriptor.type === "file") state.live--;
      audits.delete(verifyDisposed);
    };
    const verifyDisposed = async () => {
      const refused = await descriptor.stat().then(
        () => false,
        (error: unknown) => error instanceof FilesystemError && error.code === "bad-descriptor",
      );
      if (!refused) throw new Error("Filesystem disposal retained a usable descriptor");
      markClosed();
    };
    if (descriptor.type === "file") {
      state.opened++;
      state.live++;
      audits.add(verifyDisposed);
    }
    const unwrap = (other: Descriptor) => originals.get(other) ?? other;
    const base = {
      writable: descriptor.writable,
      stat: () => descriptor.stat(),
      sync: () => descriptor.sync(),
      isSameObject: (other: Descriptor) => descriptor.isSameObject(unwrap(other)),
      close() {
        descriptor.close();
        markClosed();
      },
    };
    const wrapped: Descriptor =
      descriptor.type === "file"
        ? ({
            ...base,
            type: "file",
            read: (offset, length) => descriptor.read(offset, length),
            write: (offset, data) => descriptor.write(offset, data),
            append: (data) => descriptor.append(data),
            setSize: (size) => descriptor.setSize(size),
            ...(descriptor.writeStream
              ? {
                  writeStream: (chunks: AsyncIterable<Uint8Array>, offset?: bigint) =>
                    descriptor.writeStream!(chunks, offset),
                }
              : {}),
          } satisfies FileDescriptor)
        : ({
            ...base,
            type: "directory",
            openAt: async (path, options) => wrap(await descriptor.openAt(path, options), audits),
            statAt: (path) => descriptor.statAt(path),
            readDirectory: () => descriptor.readDirectory(),
            createDirectoryAt: (path) => descriptor.createDirectoryAt(path),
            removeDirectoryAt: (path) => descriptor.removeDirectoryAt(path),
            unlinkFileAt: (path) => descriptor.unlinkFileAt(path),
            renameAt: (oldPath, directory, newPath) =>
              descriptor.renameAt(oldPath, unwrap(directory) as DirectoryDescriptor, newPath),
          } satisfies DirectoryDescriptor);
    originals.set(wrapped, descriptor);
    return wrapped as T;
  }
  return {
    state,
    provider: {
      open(mount: string, writable: boolean) {
        const instance = provider.open(mount, writable);
        const audits = new Set<() => Promise<void>>();
        return {
          writable: instance.writable,
          openRoot: () => wrap(instance.openRoot(), audits),
          async dispose() {
            await instance.dispose();
            // A mount closes its own descriptors directly, bypassing wrapper.close(). Verify
            // those real descriptors are unusable before accounting for their release.
            await Promise.all([...audits].map((verify) => verify()));
          },
        };
      },
    } satisfies FilesystemProvider,
  };
}

/**
 * Run the standard WASI source scenario and its boundary probes on supplied memory and actual
 * OPFS providers: raw non-UTF-8 bytes, exact u64 offsets, retained descriptors, external abort,
 * denied grants, disposed resources, isolated descriptor indices and cleanup on retirement.
 */
export async function verifyStandardScenario(): Promise<string[]> {
  // Every part runs; failures are reported together so one cannot hide another.
  const report: string[] = [];
  const failures: unknown[] = [];
  const part = async (name: string, run: () => Promise<string>) => {
    try {
      report.push(await run());
    } catch (error) {
      failures.push(new Error(`${name}: ${String(error)}`, { cause: error }));
    }
  };
  await part("system", systemOn);
  await part("http", verifyHttpScenario);
  await part("raw http", verifyRawHttpScenario);
  await part("sockets", verifySocketScenario);
  const pattern = Uint8Array.from({ length: 4100 }, (_, index) => index % 256);
  const root = await navigator.storage.getDirectory();
  const directoryName = `tembra-standard-${crypto.randomUUID()}`;
  const directory = await root.getDirectoryHandle(directoryName, {
    create: true,
  });
  const writable = await (
    await directory.getFileHandle("bytes.bin", { create: true })
  ).createWritable();
  await writable.write(pattern);
  await writable.close();
  const large = Uint8Array.from({ length: 3 * (1 << 20) + 5 }, (_, index) => index % 251);
  const largeWriter = await (
    await directory.getFileHandle("large.bin", { create: true })
  ).createWritable();
  await largeWriter.write(large);
  await largeWriter.close();
  try {
    for (const [kind, base] of [
      ["memory", memoryFilesystem({ "bytes.bin": pattern, "large.bin": large })],
      ["opfs", await opfsFilesystem(directory)],
    ] as const) {
      const { state, provider } = counted(base);
      await part(`${kind} standard`, async () => `${kind}: ${await standardOn(provider, state)}`);
      await part(
        `${kind} filesystem`,
        async () => `${kind} filesystem: ${await filesystemOn(provider, kind)}`,
      );
    }
    const injected = counted(memoryFilesystem());
    report.push(`injected filesystem: ${await filesystemOn(injected.provider, "memory")}`);
    if (injected.state.opened < 1 || injected.state.live !== 0)
      throw new Error("Injected provider did not observe balanced file ownership");
  } finally {
    await root.removeEntry(directoryName, { recursive: true });
  }
  if (failures.length)
    throw new AggregateError(failures, failures.map((failure) => String(failure)).join("; "));
  return report;
}

/** The same authored TypeScript exercises the full filesystem on each host adapter. */
async function filesystemOn(
  provider: FilesystemProvider,
  kind: "memory" | "opfs",
): Promise<string> {
  const host = await createHost({
    isolated: { providers: { filesystem: { data: provider } } },
  });
  let outcome: string;
  try {
    await host.install({
      artifact: await source("filesystem"),
      grants: {
        filesystem: [{ provider: "data", mount: "/data", writable: true }],
      },
    });
    const reference = await host.get("filesystem::example:filesystem/scenario@1.0.0", "main");
    try {
      const result = JSON.parse(await reference.roundtrip());
      const expected = {
        length: 150006,
        listing: ["renamed.bin"],
        times: "unsupported",
        links: "unsupported",
        symlinks: "unsupported",
        directoryRename: kind === "opfs" ? "nested" : "moved",
      };
      if (JSON.stringify(result) !== JSON.stringify(expected))
        throw new Error(`Filesystem roundtrip: ${JSON.stringify(result)}`);
      outcome = JSON.stringify(result);
    } finally {
      reference.release();
    }
  } finally {
    await host.close();
  }
  if (host.diagnostics().values !== 0 || host.diagnostics().pendingJobs !== 0)
    throw new Error("Filesystem roundtrip retained runtime values or jobs");
  return outcome;
}

async function standardOn(
  provider: FilesystemProvider,
  state: { opened: number; live: number },
): Promise<string> {
  const same = (actual: unknown, expected: unknown, label: string) => {
    if (JSON.stringify(actual) !== JSON.stringify(expected))
      throw new Error(
        `${label}: expected ${JSON.stringify(expected)}, received ${JSON.stringify(actual)}`,
      );
  };
  const failure = async (operation: Promise<unknown>): Promise<string> => {
    try {
      await operation;
    } catch (error) {
      return String(error);
    }
    throw new Error("Forbidden operation succeeded");
  };
  const filesystem = [{ provider: "data", mount: "/data", writable: false }];
  const grants = {
    system: ["clocks", "random"] as ("clocks" | "random")[],
    filesystem,
  };
  const probing = { filesystem };
  const host = await createHost({
    isolated: { providers: { filesystem: { data: provider } } },
  });
  const artifacts = {
    standard: await source("standard"),
    boundary: await source("boundary"),
    peer: await source("boundary-peer"),
  };
  const timings: string[] = [];
  const timed = async <T>(label: string, operation: () => Promise<T>): Promise<T> => {
    const started = performance.now();
    const value = await operation();
    timings.push(`${label} ${(performance.now() - started).toFixed(1)}ms`);
    return value;
  };
  try {
    // Denied grants refuse installation before any guest heap or descriptor exists.
    const ungranted = await failure(
      host.install({ artifact: artifacts.standard, grants: { filesystem } }),
    );
    const unmounted = await failure(
      host.install({
        artifact: artifacts.standard,
        grants: { system: grants.system },
      }),
    );
    same(state.opened, 0, "denied installs opened files");
    await host.install([
      { artifact: artifacts.standard, grants },
      { artifact: artifacts.boundary, grants: probing },
      { artifact: artifacts.peer, grants: probing },
    ]);
    const scenario = await host.get("standard::example:standard/scenario@1.0.0", "main");
    same(
      JSON.parse(await timed("probe", () => scenario.probe())),
      {
        randomLength: 16,
        randomIsBytes: true,
        wordIsBigInt: true,
        waitedAtLeast5ms: true,
        mount: "/data",
        type: "regular-file",
        size: "4100",
        length: 4100,
        pattern: true,
        beyond: 0,
      },
      "probe",
    );
    const reused = JSON.stringify({ size: "4100", tail: [0, 1, 2, 3] });
    same(await timed("reuse", () => scenario.reuse()), reused, "reuse");
    const streamed = JSON.parse(await timed("large", () => scenario.large()));
    if (
      streamed.length !== 3 * (1 << 20) + 5 ||
      streamed.chunks < 2 ||
      streamed.largest > 65536 ||
      !streamed.matches ||
      streamed.done !== "ok"
    )
      throw new Error(`large: ${JSON.stringify(streamed)}`);
    // External abort of a pending standard wait settles the callback, not the retained file.
    const aborted = await timed("external abort", () =>
      failure(
        host.root(async (context) => {
          const pending = scenario.hang(context);
          setTimeout(() => context.cancel(), 20);
          await pending;
        }),
      ),
    );
    // Standard clock cancellation ends its wait early, as on the native host; the caller's
    // cancelled invocation does not revoke this revision's retained descriptors.
    same(await scenario.reuse(), reused, "retained descriptor after clock cancellation");
    const started = performance.now();
    same(await timed("abandon", () => scenario.abandon()), "returned", "abandon");
    if (performance.now() - started < 30)
      throw new Error("Detached standard work ended before its physical wait settled");
    same(await timed("reuse after abandon", () => scenario.reuse()), reused, "reuse after abandon");
    same(
      JSON.parse(await scenario.denied()),
      {
        traversal: "not-permitted",
        write: "not-permitted",
        create: "not-permitted",
        missing: "no-entry",
      },
      "denied",
    );
    const boundary = await host.get("boundary::example:standard/probes@1.0.0", "main");
    const peer = await host.get("boundary-peer::example:standard/probes@1.0.0", "main");
    const probes = JSON.parse(await boundary.probes());
    same(probes.seams, [], "private packet seams are absent");
    if (!/disposed|dropped|transferred/i.test(probes.droppedObject))
      throw new Error(`Disposed descriptor was not refused: ${probes.droppedObject}`);
    const own = await boundary.descriptor();
    same(await boundary.inspect(own), "accepted", "own descriptor");
    await peer.descriptor();
    const peerOnly = await peer.descriptor();
    same(await peer.inspect(peerOnly), "accepted", "peer descriptor");
    same(await boundary.inspect(peerOnly), "unknown descriptor", "descriptor indices stay local");
    // Retirement: reloading drops the old revision's handles and closes its opened files.
    const live = state.live;
    if (live < 1) throw new Error("The retained descriptor is not open before retirement");
    await host.reload([{ artifact: artifacts.standard, grants }]);
    const after = await failure(scenario.reuse());
    if (!/unreachable|component trap/.test(after))
      throw new Error(`The fresh revision did not reject its missing descriptor: ${after}`);
    same(
      (await failure(scenario.reuse())).includes("quarantined"),
      true,
      "the uncaught missing-descriptor error quarantines its fresh revision",
    );
    // The retired revision is disposed once no reference pins its scopes; only its file closes.
    const unpinned = state.live;
    scenario.release();
    const retiring = performance.now();
    // Boundary probes retain preopened directories; every opened file must close on retirement.
    while (state.live !== 0 && performance.now() - retiring < 2000)
      await new Promise((resolve) => setTimeout(resolve, 10));
    same(state.live, 0, "files open after the retired revision was released");
    const retired = {
      live: state.live,
      ms: Math.round(performance.now() - retiring),
    };
    for (const reference of [boundary, peer]) reference.release();
    await host.close();
    same(state.live, 0, "files open after host close");
    return JSON.stringify({
      ungranted: ungranted.slice(0, 120),
      unmounted: unmounted.slice(0, 120),
      aborted: aborted.slice(0, 120),
      large: streamed,
      openedFiles: state.opened,
      liveBeforeReload: live,
      liveAfterReloadWhilePinned: unpinned,
      afterRetirement: retired,
      timings,
    });
  } finally {
    await host.close();
  }
}

/** The authored source uses configured clocks, seed, binary stdio and contained exit in Chrome. */
async function systemOn(): Promise<string> {
  const input = Uint8Array.from({ length: 131075 }, (_, index) => index % 251);
  const output: Uint8Array[] = [],
    errors: Uint8Array[] = [];
  let monotonic = 9007199254740993n;
  let seeds = 0,
    opened = 0,
    closed = 0,
    clockReads = 0;
  const host = await createHost({
    isolated: {
      providers: {
        clocks: {
          systemNow() {
            clockReads++;
            return { seconds: 1700000001n, nanoseconds: 123456789 };
          },
          systemResolution: () => 7n,
          monotonicNow() {
            clockReads++;
            return monotonic;
          },
          monotonicResolution: () => 3n,
          async waitUntil(when, signal) {
            if (signal?.aborted) throw new Error("cancelled");
            monotonic = when;
          },
        },
        random: {
          secure: (length) => new Uint8Array(length).fill(0xab),
          insecure: (length) => new Uint8Array(length).fill(0xcd),
          insecureSeed() {
            seeds++;
            return [0x9abcdef0n, 0x12345679n];
          },
        },
        stdio() {
          opened++;
          return {
            stdout: {
              async write(bytes) {
                output.push(bytes.slice());
              },
              close() {
                closed++;
              },
            },
            stderr: {
              async write(bytes) {
                errors.push(bytes.slice());
              },
              close() {
                closed++;
              },
            },
          };
        },
      },
    },
  });
  const same = (actual: unknown, expected: unknown) => {
    if (JSON.stringify(actual) !== JSON.stringify(expected))
      throw new Error(
        `System scenario expected ${JSON.stringify(expected)}, received ${JSON.stringify(actual)}`,
      );
  };
  try {
    await host.install({ artifact: await source("ambient") });
    const policy = await host.get("ambient::example:system/policy@1.0.0", "main");
    try {
      // Without
      // the clocks and random grants the runtime's own clock reads a frozen zero and its random
      // source is a fixed non-secret sequence. Nothing is refused, and no provider is touched.
      const frozen = JSON.parse(await policy.inspect());
      same([frozen.now, frozen.date, frozen.constructor], [0, 0, 0]);
      same(Date.parse(frozen.call), 0);
      same(frozen.pure, "1970-01-01T00:00:00.000Z");
      if (typeof frozen.random !== "number" || frozen.random < 0 || frozen.random >= 1)
        throw new Error(`Frozen runtime random returned ${String(frozen.random)}`);
    } finally {
      policy.release();
    }
    same([seeds, opened, clockReads], [0, 0, 0]);
    let refused = false;
    try {
      await host.install({
        artifact: await source("system"),
        grants: { system: ["clocks", "random"] },
      });
    } catch {
      refused = true;
    }
    if (!refused) throw new Error("System Orb installed without CLI permission");
    same([seeds, opened, clockReads], [0, 0, 0]);
    await host.install({
      artifact: await source("system"),
      grants: { system: ["clocks", "random", "cli"] },
      cli: {
        arguments: ["probe", "--bytes"],
        environment: [["MODE", "test"]],
        initialCwd: "/data",
        stdin: { bytes: input },
      },
    });
    const scenario = await host.get("system::example:system/scenario@1.0.0", "main");
    try {
      const result = JSON.parse(await scenario.inspect());
      same(result.wall, ["1700000001", 123456789]);
      same(result.resolution, "7");
      same(result.monotonic, "9007199254740993");
      same(result.monotonicResolution, "3");
      same(result.secureLength, 16);
      same(result.insecureLength, 17);
      same(result.secureU64, 0xababababababababn.toString());
      same(result.insecureU64, 0xcdcdcdcdcdcdcdcdn.toString());
      same(result.seed, ["2596069104", "305419897"]);
      // The runtime's private random source always returns a value in [0, 1).
      if (typeof result.random !== "number" || !(result.random >= 0 && result.random < 1))
        throw new Error(`Math.random returned ${String(result.random)}`);
      same([seeds, opened, result.seedStable, result.terminal], [1, 1, true, true]);
      same(result.date, 1700000001123);
      same(result.arguments, ["probe", "--bytes"]);
      same(result.environment, [["MODE", "test"]]);
      same(result.cwd, "/data");
      await scenario.wait(1);
      same(JSON.parse(await scenario.transfer()), {
        length: input.length,
        first: 0,
        last: input[input.length - 1],
      });
      const all = new Uint8Array(output.reduce((length, bytes) => length + bytes.length, 0));
      let offset = 0;
      for (const bytes of output) {
        all.set(bytes, offset);
        offset += bytes.length;
      }
      if (all.length !== input.length || !all.every((value, index) => value === input[index]))
        throw new Error("Binary stdout changed bytes");
      same(Array.from(errors[0]!), [0, 255, 128, 10]);
      let exited = false;
      try {
        await scenario.stop(7);
      } catch (error) {
        exited = String(error).includes("orb exited with status 7");
      }
      if (!exited) throw new Error("Guest caught CLI exit or lost its exact status");
      let retired = false;
      try {
        await scenario.inspect();
      } catch {
        retired = true;
      }
      if (!retired) throw new Error("Exited revision was reused");
    } finally {
      scenario.release();
    }
  } finally {
    await host.close();
  }
  same(closed, 2);
  same([host.diagnostics().values, host.diagnostics().pendingJobs], [0, 0]);
  return "system: deterministic clocks/random, binary stdio, frozen ambient authority and contained exit passed";
}
