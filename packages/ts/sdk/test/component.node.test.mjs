import { spawn, spawnSync } from "node:child_process";
import { missing } from "./skip-reason.mjs";
import { once } from "node:events";
import { createRequire } from "node:module";
import { test } from "node:test";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import path from "node:path";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { createHost, directoryFilesystem } from "../dist/node.js";
import { signature } from "../dist/runtime/adapters.js";
import { dependencies } from "./component-dependencies.mjs";

// The no-std probe component imports WASI 0.3 through its generated WIT bindings.
const probe = process.env.TEMBRA_WASM_PROBE;
// The no-std pipeline component imports two other Orbs, one through P3 async lowering.
const consumer = process.env.TEMBRA_WASM_CONSUMER;
const service = "example:probe/probe@1.0.0";
const wit = path.resolve(import.meta.dirname, "../../../../crates/executor-wasm/tests/wit");
const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");

/** Mirror the guest's wrapping u32 checksum. */
function checksum(rounds) {
  let value = 0;
  for (let round = 0; round < rounds; round += 1) value = (Math.imul(value, 31) + round) >>> 0;
  return value;
}

/** Package a raw component or closed source bundle as one Tembra artifact. */
async function artifact(directory, world, name, { component, source } = {}) {
  const resolved = new binding.NodeContract(path.join(wit, directory), world);
  const bytes = component === undefined ? undefined : new Uint8Array(await readFile(component));
  return new Uint8Array(
    source === undefined
      ? resolved.componentArtifact(bytes, name, 1)
      : resolved.sourceArtifact(source, name, 1),
  );
}

/** Package the probe under one Orb name. */
function probeArtifact(name) {
  return artifact("probe", "probe-provider", name, { component: probe });
}

/** Run `operation` in a scratch directory that is always removed. */
async function scratch(operation) {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-node-component-"),
  );
  try {
    return await operation(directory);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test(
  "compiled component calls wait on their worker, not on Node's libuv pool",
  { skip: missing("TEMBRA_WASM_PROBE") || process.platform === "win32", timeout: 60000 },
  () =>
    scratch(async (directory) => {
      const fifo = path.join(directory, "pipe");
      await writeFile(path.join(directory, "input.txt"), "initial");
      assert.equal(spawnSync("mkfifo", [fifo]).status, 0);
      const host = createHost({
        isolated: { providers: { filesystem: { data: directoryFilesystem(directory) } } },
      });
      let release;
      try {
        await host.install({
          artifact: await probeArtifact("probe"),
          grants: {
            system: ["clocks"],
            filesystem: [{ provider: "data", mount: "/data", writable: false }],
          },
        });
        const reference = await host.get(`probe::${service}`, "main");
        // Refused input settles without poisoning the Store.
        await assert.rejects(reference.spin("x"));
        assert.equal(await reference.spin(3), checksum(3));

        // Occupy more calls than libuv has threads; the first blocks inside a WASI open.
        const threads = Number(process.env.UV_THREADPOOL_SIZE) || 4;
        let settled = 0;
        const track = (promise) => promise.finally(() => (settled += 1));
        const blocked = track(reference.open("/data/pipe"));
        const queued = Array.from({ length: threads }, (_, index) =>
          track(reference.spin(1000 + index)),
        );
        let timer;
        try {
          const text = await Promise.race([
            readFile(path.join(directory, "input.txt"), "utf8"),
            new Promise((_, reject) => {
              timer = setTimeout(
                () => reject(new Error("component calls blocked Node's worker pool")),
                1000,
              );
            }),
          ]);
          assert.equal(text, "initial");
          assert.equal(settled, 0, "component calls settled before their WASI work was released");
        } finally {
          clearTimeout(timer);
          // A child writer opens the FIFO without using the libuv pool.
          release = spawn("sh", ["-c", ': > "$1"', "release", fifo], { stdio: "ignore" });
        }
        assert.equal((await blocked).tag, "ok");
        const results = await Promise.all(queued);
        results.forEach((value, index) => assert.equal(value, checksum(1000 + index)));
        reference.release();
      } finally {
        if (release && release.exitCode === null) {
          const exited = once(release, "exit");
          const kill = setTimeout(() => release.kill(), 5000);
          await exited;
          clearTimeout(kill);
        }
        await host.close();
      }
    }),
);

test(
  "a terminal component failure settles every queued call with the quarantine",
  { skip: missing("TEMBRA_WASM_PROBE"), timeout: 60000 },
  () =>
    scratch(async (directory) => {
      const host = createHost({
        isolated: {
          limits: { timeoutMs: 200, fuel: 2 ** 40 },
          providers: { filesystem: { data: directoryFilesystem(directory) } },
        },
      });
      try {
        await host.install({
          artifact: await probeArtifact("busy"),
          grants: {
            system: ["clocks"],
            filesystem: [{ provider: "data", mount: "/data", writable: false }],
          },
        });
        const reference = await host.get(`busy::${service}`, "main");
        const calls = [reference.spin(4294967295), reference.spin(1), reference.spin(2)];
        for (const result of await Promise.allSettled(calls)) {
          assert.equal(result.status, "rejected");
          assert.match(result.reason.message, /timed out/);
        }
        await assert.rejects(reference.spin(1), /timed out/);
        reference.release();
      } finally {
        await host.close();
      }
    }),
);

test(
  "the published Node host installs the compiled probe with a filesystem grant",
  { skip: missing("TEMBRA_WASM_PROBE"), timeout: 60000 },
  () =>
    scratch(async (directory) => {
      const host = createHost({
        isolated: { providers: { filesystem: { data: directoryFilesystem(directory) } } },
      });
      try {
        await writeFile(path.join(directory, "input.txt"), "initial");
        await host.install({
          artifact: await probeArtifact("probe"),
          grants: {
            system: ["clocks"],
            filesystem: [{ provider: "data", mount: "/data", writable: true }],
          },
        });
        const reference = await host.get(`probe::${service}`, "main");
        assert.equal(await reference.spin(1000), checksum(1000));
        assert.equal((await reference.open("/data/input.txt")).tag, "ok");
        assert.equal((await reference["create-directory"]("/data/made")).tag, "ok");
        assert.equal((await reference.open("/data/../outside.txt")).tag, "err");
        reference.release();
      } finally {
        await host.close();
      }
    }),
);

/** The structured fields of a call's error. */
async function refused(call) {
  try {
    await call();
  } catch ({ code, reason, message }) {
    return { code, reason, message };
  }
  assert.fail("the call succeeded");
}

/**
 * Run `script` on the JS component adapter with `bytes` prepared for it, and read the JSON line
 * it prints per call. The prepared glue needs JSPI: a child Node enables it, and resolves the
 * adapter's SDK import to this package.
 */
async function inAdapter(directory, bytes, script, ...args) {
  const { prepareBrowserComponent } = await import("../dist/tooling/index.js");
  const digest = createHash("sha256").update(bytes).digest("hex");
  const adapter = await prepareBrowserComponent(bytes, digest, path.join(directory, "adapter"));
  await mkdir(path.join(directory, "node_modules/@syntropika"), { recursive: true });
  await symlink(
    path.resolve(import.meta.dirname, ".."),
    path.join(directory, "node_modules/@syntropika/tembra-sdk"),
  );
  const artifactFile = path.join(directory, "artifact.wasm");
  await writeFile(artifactFile, bytes);
  const child = spawnSync(
    process.execPath,
    [
      "--experimental-wasm-jspi",
      path.join(import.meta.dirname, script),
      adapter.module,
      artifactFile,
      ...args,
    ],
    { encoding: "utf8", timeout: 60000 },
  );
  assert.equal(child.status, 0, child.stderr);
  return child.stdout
    .trim()
    .split("\n")
    .map((line) => JSON.parse(line));
}

test(
  "a trap answers the same reasons in the native worker and in the JS component adapter",
  { skip: missing("TEMBRA_WASM_PROBE"), timeout: 120000 },
  () =>
    scratch(async (directory) => {
      const bytes = await probeArtifact("probe");
      const grants = { filesystem: [{ provider: "data", mount: "/data", writable: false }] };
      // The native worker runs the compiled probe in the Node host.
      const host = createHost({
        isolated: { providers: { filesystem: { data: directoryFilesystem(directory) } } },
      });
      let native;
      try {
        await host.install({ artifact: bytes, grants });
        const reference = await host.get(`probe::${service}`, "main");
        native = [await refused(() => reference.trap()), await refused(() => reference.spin(1))];
        reference.release();
      } finally {
        await host.close();
      }
      const adapted = await inAdapter(directory, bytes, "component-trap-adapter.mjs");
      for (const [trap, quarantined] of [native, adapted]) {
        const answer = (reason) => ({ code: "unavailable", reason, message: trap.message });
        assert.deepEqual(trap, answer("wasm_executor"));
        assert.match(trap.message, /^component trap: /);
        // Every later entry is refused with the trap that quarantined the revision.
        assert.deepEqual(quarantined, answer("wasm_executor_quarantined"));
      }
    }),
);

const pipeline = "example:pipeline/pipeline@1.0.0";
const requires = {
  "example:pipeline/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 },
  "example:pipeline/delayed@1.0.0": { orb: "delayed", apiVersion: 1 },
};

/** A closed source Orb whose single method waits before doubling. */
function delayedSource(world, name, alias) {
  return artifact("pipeline", world, name, {
    source: `globalThis.__tembraOrb={services:{${alias}:{methods:{async double(_e,_c,value){await tembra.wait(40);return value*2;}}}}};`,
  });
}

test(
  "a compiled Orb imports delayed TypeScript Orbs through the Node host",
  { skip: missing("TEMBRA_WASM_CONSUMER"), timeout: 60000 },
  async () => {
    const host = createHost();
    try {
      await host.install([
        { artifact: await delayedSource("arithmetic-provider", "arithmetic", "arithmetic") },
        { artifact: await delayedSource("delayed-provider", "delayed", "delayed") },
        {
          artifact: await artifact("pipeline", "consumer", "pipeline", { component: consumer }),
          requires,
        },
      ]);
      const reference = await host.get(`pipeline::${pipeline}`, "main");
      const started = performance.now();
      // Synchronous WIT imports: the guest blocks while the source provider waits.
      assert.equal(await reference.quadruple(6), 24);
      // P3 async imports awaited from an async export.
      assert.equal(await reference.octuple(3), 24);
      assert.ok(performance.now() - started >= 200, "every dependency result was delayed");
      reference.release();
    } finally {
      await host.close();
    }
  },
);

test(
  "a TypeScript dependency's refusal traps its compiled caller alike in both hosts",
  { skip: missing("TEMBRA_WASM_CONSUMER"), timeout: 120000 },
  () =>
    scratch(async (directory) => {
      const bytes = await artifact("pipeline", "consumer", "pipeline", { component: consumer });
      // The WIT digest the pipeline requires of each dependency.
      const plan = JSON.parse(
        new binding.NodeArtifact(bytes).request(
          JSON.stringify({
            kind: "install",
            name: "pipeline",
            apiVersion: 1,
            requires,
            grants: {},
            filesystem: [],
            network: [],
          }),
        ),
      ).ok.plan;
      const signatures = Object.fromEntries(
        plan.orb.requires.map(({ orb, signatures }) => [orb, Object.values(signatures)[0]]),
      );
      const host = createHost();
      let native;
      try {
        await host.install([...dependencies(signatures, signature), { artifact: bytes, requires }]);
        const reference = await host.get(`pipeline::${pipeline}`, "main");
        native = [
          await refused(() => reference.quadruple(1)),
          await refused(() => reference.quadruple(1)),
        ];
        reference.release();
      } finally {
        await host.close();
      }
      const adapted = await inAdapter(
        directory,
        bytes,
        "component-dependency-adapter.mjs",
        JSON.stringify(signatures),
      );
      // The native worker's trap text names the Wasm frame first; both end with the refusal.
      const refusal = "dependency call failed: released/reference: The reference has been released";
      for (const [trap, quarantined] of [native, adapted]) {
        assert.equal(trap.reason, "wasm_executor", trap.message);
        assert.match(trap.message, /^component trap: /);
        assert.ok(trap.message.endsWith(refusal), trap.message);
        assert.deepEqual(quarantined, { ...trap, reason: "wasm_executor_quarantined" });
      }
      assert.equal(adapted[0].message, `component trap: ${refusal}`);
    }),
);
