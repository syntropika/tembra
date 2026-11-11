/** Actual compiled components qualifying WASI clocks, randomness and CLI in Chrome. */
import { createHost, type PreparedComponent } from "../src/browser.ts";
import type { BrowserInterpreterOptions } from "../src/interpreter/browser.ts";
import type { OrbArtifact } from "../src/authoring/types.ts";
import type { Host } from "../src/host.ts";

type Reference = { release(): void; [method: string]: any };
const SERVICE = "compiled-system::example:system/probe@1.0.0";
const GRANTS = { system: ["clocks", "random", "cli"] as const };
const MAX = (1n << 64n) - 1n;
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
function equal(actual: unknown, expected: unknown, label: string): void {
  const encode = (value: unknown) =>
    JSON.stringify(value, (_key, item) => (typeof item === "bigint" ? `${item}n` : item));
  if (encode(actual) !== encode(expected))
    throw new Error(`${label}: expected ${encode(expected)}, got ${encode(actual)}`);
}
function deferred<T = void>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((accept) => {
    resolve = accept;
  });
  return { promise, resolve };
}
async function refused(operation: Promise<unknown>, label: string): Promise<Error> {
  const result = await operation.catch((error: unknown) => error);
  if (!(result instanceof Error)) throw new Error(`${label} unexpectedly succeeded`);
  return result;
}
async function artifact(): Promise<Uint8Array> {
  const response = await fetch("/compiled-system.wasm");
  if (!response.ok) throw new Error("Missing compiled-system");
  return new Uint8Array(await response.arrayBuffer());
}
async function using(
  base: object,
  components: PreparedComponent[],
  providers: BrowserInterpreterOptions["providers"],
  options: Omit<OrbArtifact, "artifact">,
  run: (host: Host, probe: Reference) => Promise<void>,
): Promise<void> {
  const host = await createHost({
    isolated: { ...base, components, ...(providers ? { providers } : {}) },
  });
  let probe: Reference | undefined;
  try {
    await host.install({ artifact: await artifact(), ...options });
    probe = (await host.get(SERVICE, "main")) as Reference;
    await run(host, probe);
  } finally {
    probe?.release();
    await host.close();
    for (const [key, value] of Object.entries(host.diagnostics()))
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
        equal(value, 0, `${key} after system close`);
  }
}

/** Every callable clock/random import, exact 64-bit values, waits and cached revision seeds. */
export async function verifyBrowserSystemValues(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let now = 9007199254740993n;
  let seeds = 0;
  const waits: bigint[] = [];
  await using(
    base,
    components,
    {
      clocks: {
        systemNow: () => ({ seconds: -9007199254740993n, nanoseconds: 999999999 }),
        systemResolution: () => 17n,
        monotonicNow: () => now,
        monotonicResolution: () => 19n,
        async waitUntil(when) {
          waits.push(when);
          now = when;
        },
      },
      // Injected bytes make adapter conversion observable; this does not replace default CSPRNG.
      random: {
        secure: (length) => new Uint8Array(length).fill(255),
        insecure: (length) => new Uint8Array(length).fill(128),
        insecureSeed() {
          seeds++;
          return [MAX, 9007199254740993n];
        },
      },
    },
    { grants: GRANTS },
    async (host, probe) => {
      const clocks = await probe.clocks();
      equal(
        clocks,
        {
          seconds: -9007199254740993n,
          nanoseconds: 999999999,
          systemResolution: 17n,
          monotonic: now,
          monotonicResolution: 19n,
        },
        "exact clock report",
      );
      for (const length of [0n, 17n]) {
        const random = await probe.random(length);
        equal(Array.from(random.secure), new Array(Number(length)).fill(255), "secure bytes");
        equal(Array.from(random.insecure), new Array(Number(length)).fill(128), "insecure bytes");
        equal(random.secureWord, MAX, "exact secure u64");
        equal(random.insecureWord, 0x8080808080808080n, "exact insecure u64");
        equal(random.seed, [MAX, 9007199254740993n], "exact seed tuple");
        equal(random.nextSeed, random.seed, "seed stable within invocation");
      }
      equal(seeds, 1, "seed stable across roots");
      await probe["wait-until"](now + 21n);
      await probe["wait-for"](23n);
      equal(
        waits,
        [9007199254741014n, 9007199254741037n],
        "both wait imports preserve nanoseconds",
      );
      equal((await probe.clocks()).monotonic, 9007199254741037n, "wait observes same provider");
      equal(host.errors().length, 0, "no hidden system failure");
    },
  );
  // Separate compiled instances must not inherit the previous revision's cached seed.
  await using(
    base,
    components,
    {
      random: {
        secure: (length) => crypto.getRandomValues(new Uint8Array(length)),
        insecure: (length) => new Uint8Array(length),
        insecureSeed() {
          seeds++;
          return [2n, 3n];
        },
      },
    },
    { grants: GRANTS },
    async (_host, probe) => {
      equal((await probe.random(0n)).seed, [2n, 3n], "fresh instance seed");
    },
  );
  equal(seeds, 2, "one seed acquisition per instance");
}

/** Installation grants, CLI configuration capture, explicit absent terminals and contained exit. */
export async function verifyBrowserSystemIsolation(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let touched = 0;
  const host = await createHost({
    isolated: {
      ...base,
      components,
      providers: {
        clocks: {
          systemNow: () => {
            touched++;
            return { seconds: 0n, nanoseconds: 0 };
          },
          systemResolution: () => 1n,
          monotonicNow: () => 0n,
          monotonicResolution: () => 1n,
          async waitUntil() {},
        },
        random: {
          secure: (length) => {
            touched++;
            return new Uint8Array(length);
          },
          insecure: (length) => new Uint8Array(length),
          insecureSeed: () => {
            touched++;
            return [1n, 2n];
          },
        },
        stdio: () => {
          touched++;
          return {};
        },
      },
    },
  });
  try {
    for (const denied of [
      [],
      ["clocks", "random"],
      ["clocks", "cli"],
      ["random", "cli"],
    ] as const) {
      await refused(
        host.install({ artifact: await artifact(), grants: { system: denied } }),
        `missing grant ${denied}`,
      );
      equal(touched, 0, "missing grant refuses before provider access");
    }
  } finally {
    await host.close();
  }
  const args = ["probe", "--flag=ü"];
  const environment = [["MODE", "va=lue"]] as [string, string][];
  await using(
    base,
    components,
    undefined,
    { grants: GRANTS, cli: { arguments: args, environment, initialCwd: "/private/work" } },
    async (_host, probe) => {
      args[0] = "mutated";
      environment[0]![1] = "mutated";
      const report = await probe.environment();
      equal(report.arguments, ["probe", "--flag=ü"], "captured arguments");
      equal(report.environment, [["MODE", "va=lue"]], "captured environment");
      equal(report.cwd, "/private/work", "configured cwd");
      equal(report.terminals, [true, true, true], "all three terminal getters return none");
    },
  );
  for (const [method, argument, status] of [
    ["stop", 7, 7],
    ["exit", true, 0],
    ["exit", false, 1],
  ] as const)
    await using(base, components, undefined, { grants: GRANTS }, async (host, probe) => {
      const report = await probe.environment();
      equal(report.environment, [], "environment never inherited");
      equal(report.arguments, [], "arguments never inherited");
      equal(report.cwd, null, "cwd never inherited");
      await host.install({
        name: "healthy-peer",
        apiVersion: 1,
        services: { "healthy-peer.alive": { methods: { ping: () => "healthy" } } },
      });
      const peer = await host.get("healthy-peer.alive", "main");
      try {
        const error = await refused(probe[method](argument), `CLI ${method}`);
        if (!String(error).includes(`orb exited with status ${status}`)) throw error;
        await refused(probe.ping(), "exited component reentry");
        equal(await peer.ping(), "healthy", "exit contains quarantine to its own Orb");
      } finally {
        peer.release();
      }
    });
}

/** Binary stdin/stdout/stderr, provider errors, reader drop and output backpressure. */
export async function verifyBrowserSystemStreams(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  const bytes = Uint8Array.from({ length: 131075 }, (_, index) => index % 251);
  const stdout: number[] = [],
    stderr: number[] = [];
  let offset = 0,
    closed = 0;
  await using(
    base,
    components,
    {
      stdio: () => ({
        stdin: {
          async read(limit) {
            const part = bytes.slice(offset, offset + Math.min(limit, 10003));
            offset += part.length;
            return part.length ? part : undefined;
          },
          close() {
            closed++;
          },
        },
        stdout: {
          async write(chunk) {
            for (const byte of chunk) stdout.push(byte);
          },
          close() {
            closed++;
          },
        },
        stderr: {
          async write(chunk) {
            for (const byte of chunk) stderr.push(byte);
          },
          close() {
            closed++;
          },
        },
      }),
    },
    { grants: GRANTS },
    async (host, probe) => {
      const report = (await probe.transfer()).val;
      let checksum = 0n;
      for (const byte of bytes) checksum = (checksum * 31n + BigInt(byte)) & MAX;
      equal(report, { bytes: BigInt(bytes.length), checksum }, "binary transfer report");
      equal(stdout, Array.from(bytes), "stdout preserves non-UTF8 bytes across chunks");
      equal(stderr, [0, 255, 128, 10], "stderr bytes");
      equal(host.errors().length, 0, "normal stdio does not report failures");
    },
  );
  equal(closed, 3, "providers close exactly once");
  await using(
    base,
    components,
    undefined,
    { grants: GRANTS, cli: { stdin: { bytes } } },
    async (_host, probe) => {
      equal(
        await probe["read-drop"](),
        { tag: "ok", val: 1 },
        "stdin early reader drop completes successfully",
      );
    },
  );
  for (const failing of ["stdin", "stdout", "stderr"] as const)
    await using(
      base,
      components,
      {
        stdio: () => ({
          [failing]:
            failing === "stdin"
              ? {
                  async read() {
                    throw new Error("read failed");
                  },
                }
              : {
                  async write() {
                    throw new Error("write failed");
                  },
                },
        }),
      },
      { grants: GRANTS },
      async (_host, probe) => {
        const result = await (failing === "stdin"
          ? probe.transfer()
          : probe.write(failing === "stderr", new Uint8Array([0, 255, 128])));
        equal(result.tag, "err", `${failing} exposes a WIT error result`);
        equal(await probe.ping(), "alive", "I/O error is a WIT error, not quarantine");
      },
    );
  const gate = deferred(),
    started = deferred();
  let settled = false;
  await using(
    base,
    components,
    {
      stdio: () => ({
        stdout: {
          async write() {
            started.resolve();
            await gate.promise;
          },
        },
      }),
    },
    { grants: GRANTS },
    async (_host, probe) => {
      const writing = probe.write(false, bytes).then(() => {
        settled = true;
      });
      await started.promise;
      await tick();
      equal(settled, false, "guest output waits for physical sink completion");
      gate.resolve();
      await writing;
      equal(settled, true, "drained sink releases guest");
    },
  );
}

/** A shared stdout/stderr provider must never receive overlapping physical writes. */
export async function verifyBrowserSystemOutputSerialization(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  const gate = deferred(),
    started = deferred();
  let active = 0,
    maximum = 0,
    calls = 0;
  const sink = {
    async write() {
      active++;
      maximum = Math.max(maximum, active);
      calls++;
      started.resolve();
      await gate.promise;
      active--;
    },
  };
  await using(
    base,
    components,
    { stdio: () => ({ stdout: sink, stderr: sink }) },
    { grants: GRANTS },
    async (_host, probe) => {
      const writing = probe["write-pair"](new Uint8Array([0, 255, 128]));
      await started.promise;
      // Both imports are admitted before the guest waits for either completion.
      await new Promise((resolve) => setTimeout(resolve, 20));
      const overlapping = maximum;
      gate.resolve();
      await writing;
      equal(overlapping, 1, "one admitted physical write per shared provider");
      equal(maximum, 1, "serialized sink remains exclusive");
      equal(calls, 2, "both standard output interfaces drain");
    },
  );
}

/** Cooperative callback cancellation aborts the actual provider wait without quarantining. */
export async function verifyBrowserSystemCancellation(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  const started = deferred();
  let aborted = false;
  await using(
    base,
    components,
    {
      clocks: {
        systemNow: () => ({ seconds: 0n, nanoseconds: 0 }),
        systemResolution: () => 1n,
        monotonicNow: () => 0n,
        monotonicResolution: () => 1n,
        async waitUntil(_when, signal) {
          started.resolve();
          await new Promise<void>((_resolve, reject) => {
            const abort = () => {
              aborted = true;
              reject(new Error("cancelled"));
            };
            if (signal?.aborted) abort();
            else signal?.addEventListener("abort", abort, { once: true });
          });
        },
      },
    },
    { grants: GRANTS },
    async (host, probe) => {
      await host.root(async (context) => {
        const waiting = probe["wait-until"](MAX, context);
        await started.promise;
        context.cancel();
        await refused(waiting, "cancelled monotonic wait");
      });
      equal(aborted, true, "cancellation reached clock provider");
      equal(await probe.ping(), "alive", "cooperative wait cancellation preserves component");
    },
  );
}

/** Pending physical stdio receives callback cancellation, not only revision retirement. */
export async function verifyBrowserSystemStdioCancellation(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  for (const direction of ["stdin", "stdout"] as const) {
    const started = deferred();
    let aborted = false;
    const block = async (signal: AbortSignal) => {
      started.resolve();
      await new Promise<void>((_resolve, reject) => {
        const stop = () => {
          aborted = true;
          reject(new Error("cancelled"));
        };
        if (signal.aborted) stop();
        else signal.addEventListener("abort", stop, { once: true });
      });
    };
    await using(
      { ...base, limits: { timeoutMs: 1000 } },
      components,
      {
        stdio: () => ({
          ...(direction === "stdin"
            ? {
                stdin: {
                  async read(_limit: number, signal: AbortSignal) {
                    await block(signal);
                    return undefined;
                  },
                },
              }
            : {
                stdout: {
                  async write(_bytes: Uint8Array, signal: AbortSignal) {
                    await block(signal);
                  },
                },
              }),
        }),
      },
      { grants: GRANTS },
      async (host, probe) => {
        await host.root(async (context) => {
          const operation =
            direction === "stdin"
              ? probe.transfer(context)
              : probe.write(false, new Uint8Array([0, 255]), context);
          await started.promise;
          context.cancel();
          await refused(operation, `cancelled physical ${direction}`);
        });
        equal(aborted, true, `${direction} provider observes callback cancellation`);
        equal(
          await probe.ping(),
          "alive",
          `${direction} cooperative cancellation avoids quarantine`,
        );
      },
    );
  }
}
