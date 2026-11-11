/** Real compiled WASI filesystem operations over memory, OPFS and a controlled host provider. */
import {
  createHost,
  memoryFilesystem,
  opfsFilesystem,
  type Descriptor,
  type FilesystemProvider,
  type PreparedComponent,
} from "../src/browser.ts";
import type { Host } from "../src/host.ts";

const SERVICE = "compiled-filesystem::example:compiled-filesystem/probe@1.0.0";
type Reference = { release(): void; [method: string]: any };
const grants = {
  filesystem: [
    { provider: "data", mount: "/data", writable: true },
    { provider: "data", mount: "/view", writable: false },
  ],
};

function equal(actual: unknown, expected: unknown, label: string): void {
  if (actual !== expected)
    throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
}
async function until(check: () => boolean, label: string): Promise<void> {
  const deadline = performance.now() + 5000;
  while (!check()) {
    if (performance.now() > deadline) throw new Error(`Timed out: ${label}`);
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}
function value<T>(result: { tag: string; val: T }): T {
  if (result.tag !== "ok") throw new Error(`Compiled filesystem: ${String(result.val)}`);
  return result.val;
}
async function artifact(): Promise<Uint8Array> {
  const response = await fetch("/compiled-filesystem.wasm");
  if (!response.ok) throw new Error("Missing compiled filesystem fixture");
  return new Uint8Array(await response.arrayBuffer());
}

/** Observe descriptor drops and physical mount retirement without replacing storage semantics. */
class Ledger {
  mounts = 0;
  descriptors = 0;
  reads = 0;
  offsets: bigint[] = [];
  pause?: () => Promise<void>;
  provider(storage: FilesystemProvider): FilesystemProvider {
    const raw = new WeakMap<object, Descriptor>();
    return {
      open: (path, writable) => {
        const mount = storage.open(path, writable);
        this.mounts++;
        const live = new Set<Descriptor>();
        const wrap = (descriptor: Descriptor): Descriptor => {
          live.add(descriptor);
          this.descriptors++;
          const proxy = new Proxy({} as Descriptor, {
            get: (_target, key) => {
              const target = descriptor;
              if (key === "close")
                return () => {
                  if (live.delete(target)) this.descriptors--;
                  target.close();
                };
              if (key === "openAt" && target.type === "directory")
                return async (...args: Parameters<typeof target.openAt>) =>
                  wrap(await target.openAt(...args));
              if (key === "read" && target.type === "file")
                return async (...args: Parameters<typeof target.read>) => {
                  this.reads++;
                  this.offsets.push(args[0]);
                  await this.pause?.();
                  return target.read(...args);
                };
              if (key === "isSameObject")
                return (other: Descriptor) => target.isSameObject(raw.get(other) ?? other);
              if (key === "renameAt" && target.type === "directory")
                return (from: string, other: Descriptor, to: string) =>
                  target.renameAt(from, (raw.get(other) ?? other) as typeof target, to);
              const member = Reflect.get(target, key);
              return typeof member === "function" ? member.bind(target) : member;
            },
          });
          raw.set(proxy, descriptor);
          return proxy;
        };
        return {
          writable: mount.writable,
          openRoot: () => wrap(mount.openRoot()) as ReturnType<typeof mount.openRoot>,
          dispose: async () => {
            await mount.dispose();
            this.descriptors -= live.size;
            live.clear();
            this.mounts--;
          },
        };
      },
    };
  }
}

async function using(
  base: object,
  components: PreparedComponent[],
  storage: FilesystemProvider,
  run: (host: Host, probe: Reference, ledger: Ledger) => Promise<void>,
): Promise<void> {
  const ledger = new Ledger();
  const host = await createHost({
    isolated: {
      ...base,
      components,
      providers: { filesystem: { data: ledger.provider(storage) } },
    },
  });
  let probe: Reference | undefined;
  try {
    await host.install({ artifact: await artifact(), grants });
    probe = (await host.get(SERVICE, "main")) as Reference;
    await run(host, probe, ledger);
    equal(host.errors().length, 0, "no hidden filesystem failures");
  } finally {
    probe?.release();
    await host.close();
    equal(ledger.mounts, 0, "all provider scopes physically disposed");
    equal(ledger.descriptors, 0, "all provider descriptors closed");
    for (const [key, count] of Object.entries(host.diagnostics()))
      if (
        [
          "objects",
          "references",
          "calls",
          "roots",
          "resources",
          "tasks",
          "values",
          "pendingJobs",
        ].includes(key)
      )
        equal(count, 0, `${key} after close`);
  }
}

async function operations(
  base: object,
  components: PreparedComponent[],
  storage: FilesystemProvider,
  kind: "memory" | "opfs",
): Promise<void> {
  await using(base, components, storage, async (_host, probe, ledger) => {
    const report = value<{
      length: bigint;
      largeOffset: string;
      listing: string[];
      directoryRename: string;
      hashLower: bigint;
      hashUpper: bigint;
    }>(await probe.roundtrip("/data"));
    equal(report.length, 150006n, "binary byte count");
    equal(report.largeOffset, "eof", "full-u64 read offset stays beyond EOF");
    equal(report.listing.join(","), "renamed.bin", "typed directory-entry stream");
    equal(
      report.directoryRename,
      kind === "opfs" ? "nested" : "moved",
      "directory rename capability",
    );
    equal(typeof report.hashLower, "bigint", "metadata lower u64");
    equal(typeof report.hashUpper, "bigint", "metadata upper u64");
    if (!ledger.offsets.includes((1n << 64n) - 8n))
      throw new Error("Compiled read offset lost u64 precision before reaching the provider");
    const errors = value<{
      checked: string[];
      hardLink: string;
      symlink: string;
      timestamp: string;
      timestampAt: string;
      timestampMissing: string;
    }>(await probe.errors("/data", "/view"));
    const expectedErrors = [
      "missing:ErrorCode::NoEntry",
      "exclusive:ErrorCode::Exist",
      "traversal:../escape:ErrorCode::NotPermitted",
      "traversal:/escape:ErrorCode::NotPermitted",
      "traversal:child/../../escape:ErrorCode::NotPermitted",
      "nul:ErrorCode::Invalid",
      "wrong-kind:ErrorCode::NotDirectory",
      "readonly-file-size:ErrorCode::BadDescriptor",
      "readonly-file-times:ErrorCode::BadDescriptor",
      "read-directory-as-file:ErrorCode::BadDescriptor",
      "read-file-as-directory:ErrorCode::NotDirectory",
      "readonly-directory:ErrorCode::NotPermitted",
      "readonly-open:ErrorCode::NotPermitted",
      "readlink-file:ErrorCode::Invalid",
      "readlink-missing:ErrorCode::NoEntry",
    ];
    equal(errors.checked.join("\n"), expectedErrors.join("\n"), "exact standard browser errors");
    equal(errors.timestampMissing, "ErrorCode::NoEntry", "missing no-change timestamp result");
    for (const capability of ["hardLink", "symlink", "timestamp", "timestampAt"] as const)
      equal(errors[capability], "ErrorCode::Unsupported", `${capability} standard browser outcome`);
    equal(
      errors.checked.filter((entry) => entry.startsWith("traversal:")).length,
      3,
      "lexical confinement",
    );
    equal(ledger.descriptors, 0, "all ordinary handles dropped before scope retirement");
  });
}

/** All supported descriptor and path methods against actual in-memory storage. */
export async function verifyCompiledFilesystemMemory(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await operations(base, components, memoryFilesystem({ "input.txt": "seeded" }), "memory");
}

/** The same compiled calls against actual browser OPFS, including precise unsupported outcomes. */
export async function verifyCompiledFilesystemOpfs(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  const root = await navigator.storage.getDirectory();
  const name = `tembra-compiled-filesystem-${crypto.randomUUID()}`;
  const directory = await root.getDirectoryHandle(name, { create: true });
  try {
    const writer = await (
      await directory.getFileHandle("input.txt", { create: true })
    ).createWritable();
    await writer.write("seeded");
    await writer.close();
    await operations(base, components, await opfsFilesystem(directory), "opfs");
  } finally {
    await root.removeEntry(name, { recursive: true });
  }
}

/** A stream retains its descriptor; drops, reload and cancelled physical reads release it. */
export async function verifyCompiledFilesystemLifetime(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(
    base,
    components,
    memoryFilesystem({ "input.txt": "seeded" }),
    async (host, probe, ledger) => {
      value(await probe.retain("/data"));
      equal(ledger.descriptors, 1, "stream keeps dropped file descriptor alive");
      equal(
        new TextDecoder().decode(new Uint8Array(value<number[]>(await probe["read-retained"]()))),
        "seeded",
        "read after descriptor drop",
      );
      equal(ledger.descriptors, 0, "read EOF releases retained descriptor");
      value(await probe.retain("/data"));
      await probe["drop-retained"]();
      equal(ledger.descriptors, 0, "unread stream drop releases descriptor");
      value(await probe.retain("/data"));
      await host.reload([{ artifact: await artifact(), grants }]);
      await until(() => ledger.descriptors === 0, "reload retires retained descriptor");
      const fresh = (await host.get(SERVICE, "main")) as Reference;
      try {
        let release!: () => void;
        const paused = new Promise<void>((resolve) => {
          release = resolve;
        });
        let admitted!: () => void;
        const started = new Promise<void>((resolve) => {
          admitted = resolve;
        });
        ledger.pause = () => {
          admitted();
          return paused;
        };
        try {
          await host.root(async (context) => {
            const pending = fresh["pending-read"]("/data", context).catch(
              (error: unknown) => error,
            );
            await started;
            context.cancel();
            const outcome = await pending;
            if (!(outcome instanceof Error))
              throw new Error("Cancelled read unexpectedly succeeded");
            // The provider promise is still in flight; cancellation cannot pretend it settled.
            if (ledger.descriptors < 1)
              throw new Error("Pending provider read lost its descriptor");
            release();
          });
        } finally {
          release();
          delete ledger.pause;
        }
        value(await fresh.retain("/data"));
        await fresh["drop-retained"]();
      } finally {
        fresh.release();
      }
    },
  );
}
