import {
  createHost,
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

/** Fetch one prepared qualification artifact from the test server. */
async function source(name: string): Promise<Uint8Array> {
  const response = await fetch(`/${name}.wasm`);
  if (!response.ok) throw new Error(`Missing fixture ${name}`);
  return new Uint8Array(await response.arrayBuffer());
}

/** Run one call on the selected engine; omitting `engine` uses the package-located default. */
export async function verifyEngineSelection(engine?: URL): Promise<void> {
  const host = await createHost({ isolated: engine ? { engine } : {} });
  try {
    await host.install({ artifact: await source("arithmetic-source") });
    const reference = await host.get("arithmetic::example:integrated/arithmetic@1.0.0", "main");
    const doubled = await reference.double(3);
    if (doubled !== 6) throw new Error(`Engine selection returned ${doubled}`);
    reference.release();
  } finally {
    await host.close();
  }
  if (host.diagnostics().values !== 0) throw new Error("Engine selection leaked values");
}

/** Qualify actual Wasm callbacks, isolated descriptors, OPFS persistence and bounded termination. */
export async function verifyBrowserInterpreter(engine?: URL): Promise<void> {
  await verifySetupRetry(engine);
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
      ...(engine ? { engine } : {}),
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
      {
        artifact: await source("online"),
        requires,
        grants: {
          http: [{ provider: "mock", origins: ["https://api.example.test", location.origin] }],
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
    await rejects(calculation.fail());
    await rejects(calculation.forbidden());
    await rejects(calculation.malformed());
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
    equal(await calculation.race(), -1);
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

/** A transient validator download failure must not poison later installations on the same host. */
async function verifySetupRetry(engine?: URL): Promise<void> {
  const artifact = await source("arithmetic-source");
  const host = await createHost({ isolated: engine ? { engine } : {} });
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
  function wrap<T extends Descriptor>(descriptor: T): T {
    let closed = false;
    if (descriptor.type === "file") {
      state.opened++;
      state.live++;
    }
    const unwrap = (other: Descriptor) => originals.get(other) ?? other;
    const base = {
      writable: descriptor.writable,
      stat: () => descriptor.stat(),
      sync: () => descriptor.sync(),
      isSameObject: (other: Descriptor) => descriptor.isSameObject(unwrap(other)),
      close() {
        if (!closed && descriptor.type === "file") state.live--;
        closed = true;
        descriptor.close();
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
            openAt: async (path, options) => wrap(await descriptor.openAt(path, options)),
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
        return {
          writable: instance.writable,
          openRoot: () => wrap(instance.openRoot()),
          dispose: () => instance.dispose(),
        };
      },
    } satisfies FilesystemProvider,
  };
}

/**
 * Run the standard WASI source scenario and its boundary probes on supplied memory and actual
 * OPFS providers: raw non-UTF-8 bytes, exact u64 offsets, retained descriptors, external abort,
 * denied grants, forged/foreign handles and cleanup on revision retirement.
 */
export async function verifyStandardScenario(engine?: URL): Promise<string[]> {
  const report = [
    await systemOn(engine),
    await verifyHttpScenario(engine),
    await verifyRawHttpScenario(engine),
  ];
  const pattern = Uint8Array.from({ length: 4100 }, (_, index) => index % 256);
  const root = await navigator.storage.getDirectory();
  const directoryName = `tembra-standard-${crypto.randomUUID()}`;
  const directory = await root.getDirectoryHandle(directoryName, { create: true });
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
      try {
        report.push(`${kind}: ${await standardOn(provider, state, engine)}`);
      } catch (error) {
        throw new Error(`${kind} standard scenario: ${String(error)}`, { cause: error });
      }
      try {
        report.push(`${kind} filesystem: ${await filesystemOn(provider, kind, engine)}`);
      } catch (error) {
        throw new Error(`${kind} filesystem scenario: ${String(error)}`, { cause: error });
      }
    }
    const injected = counted(memoryFilesystem());
    report.push(`injected filesystem: ${await filesystemOn(injected.provider, "memory", engine)}`);
    if (injected.state.opened < 1 || injected.state.live !== 0)
      throw new Error("Injected provider did not observe balanced file ownership");
  } finally {
    await root.removeEntry(directoryName, { recursive: true });
  }
  return report;
}

/** The same authored TypeScript exercises the full filesystem on each host adapter. */
async function filesystemOn(
  provider: FilesystemProvider,
  kind: "memory" | "opfs",
  engine?: URL,
): Promise<string> {
  const host = await createHost({
    isolated: { ...(engine ? { engine } : {}), providers: { filesystem: { data: provider } } },
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
  engine?: URL,
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
  const grants = { system: ["clocks", "random"] as ("clocks" | "random")[], filesystem };
  const probing = { filesystem };
  const host = await createHost({
    isolated: { ...(engine ? { engine } : {}), providers: { filesystem: { data: provider } } },
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
      host.install({ artifact: artifacts.standard, grants: { system: grants.system } }),
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
    same(await scenario.reuse(), reused, "reuse after external abort");
    same(await timed("abandon", () => scenario.abandon()), "returned", "abandon");
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
    same(
      JSON.parse(await boundary.probes()),
      {
        modules: ["wasi:filesystem/preopens@0.3.0", "wasi:filesystem/types@0.3.0"],
        forged: "unknown or dropped handle",
        wrongType: "handle has a different type",
        dropped: "unknown or dropped handle",
        droppedObject: "expected a live wasi:filesystem/types@0.3.0#descriptor handle",
        unimported: "standard interface is not imported by this Orb",
      },
      "boundary probes",
    );
    const own = await boundary.descriptor();
    same(await boundary.inspect(own), "accepted", "own handle");
    same(
      await boundary.inspect(await peer.descriptor()),
      "unknown or dropped handle",
      "foreign handle",
    );
    // Retirement: reloading drops the old revision's handles and closes its opened files.
    const live = state.live;
    if (live < 1) throw new Error("The retained descriptor is not open before retirement");
    await host.reload([{ artifact: artifacts.standard, grants }]);
    const after = await failure(scenario.reuse());
    if (!after.includes("no retained descriptor"))
      throw new Error(`Reloaded revision still sees a descriptor: ${after}`);
    // The retired revision is disposed once no reference pins its scopes; only its file closes.
    const unpinned = state.live;
    scenario.release();
    const retiring = performance.now();
    // The retained and large files close; only the boundary Orb's undropped probe file remains.
    while (state.live !== 1 && performance.now() - retiring < 2000)
      await new Promise((resolve) => setTimeout(resolve, 10));
    same(state.live, 1, "files open after the retired revision was released");
    const retired = { live: state.live, ms: Math.round(performance.now() - retiring) };
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
async function systemOn(engine?: URL): Promise<string> {
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
      ...(engine ? { engine } : {}),
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
      same(JSON.parse(await policy.inspect()), {
        now: true,
        date: true,
        call: true,
        constructor: true,
        random: true,
        pure: "1970-01-01T00:00:00.000Z",
      });
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
      // Same observable seed tuple and first value as the native source-builtins regression.
      same(result.random, 0.0011805466946974708);
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
  return "system: deterministic clocks/random, binary stdio, denied builtins and contained exit passed";
}
