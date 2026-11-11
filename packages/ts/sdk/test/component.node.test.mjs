import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { createRequire } from "node:module";
import { test } from "node:test";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import path from "node:path";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createHost, directoryFilesystem } from "../dist/node.js";

// The no-std probe component imports WASI 0.3 through its generated WIT bindings.
const probe = process.env.TEMBRA_WASM_PROBE;
// The no-std pipeline component imports two other Orbs, one through P3 async lowering.
const consumer = process.env.TEMBRA_WASM_CONSUMER;
// Prepared source fixtures from scripts/test-wasm-fixtures.ts.
const fixtures = process.env.TEMBRA_WASM_FIXTURES;
const service = "example:probe/probe@1.0.0";
const wit = path.resolve(import.meta.dirname, "../../../../crates/executor-wasm/tests/wit");
const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");

/** Mirror the guest's wrapping u32 checksum. */
function checksum(rounds) {
  let value = 0;
  for (let round = 0; round < rounds; round += 1) value = (Math.imul(value, 31) + round) >>> 0;
  return value;
}

/** Package a raw compiled component as one Tembra artifact. */
async function artifact(directory, world, name, { component }) {
  const resolved = new binding.NodeContract(path.join(wit, directory), world);
  return new Uint8Array(
    resolved.componentArtifact(new Uint8Array(await readFile(component)), name, 1),
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
  { skip: !probe || process.platform === "win32", timeout: 60000 },
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
  { skip: !probe, timeout: 60000 },
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
  { skip: !probe, timeout: 60000 },
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

const pipeline = "example:pipeline/pipeline@1.0.0";
const requires = {
  "example:pipeline/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 },
  "example:pipeline/delayed@1.0.0": { orb: "delayed", apiVersion: 1 },
};

/** A prepared source fixture from `scripts/test-wasm-fixtures.ts` (`tests/source/<name>`). */
async function sourceFixture(name) {
  return new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
}

test(
  "a compiled Orb imports delayed TypeScript Orbs through the Node host",
  { skip: !consumer || !fixtures, timeout: 60000 },
  async () => {
    const host = createHost();
    try {
      await host.install([
        // tests/source/test-pipeline-{arithmetic,delayed}: a synchronous WIT function is
        // implemented synchronously; only the P3 async `delayed` provider waits (40 ms per call).
        { artifact: await sourceFixture("test-pipeline-arithmetic") },
        { artifact: await sourceFixture("test-pipeline-delayed") },
        {
          artifact: await artifact("pipeline", "consumer", "pipeline", { component: consumer }),
          requires,
        },
      ]);
      const reference = await host.get(`pipeline::${pipeline}`, "main");
      const started = performance.now();
      // Synchronous WIT imports: the guest blocks on the synchronous source provider.
      assert.equal(await reference.quadruple(6), 24);
      // Three sequential P3 async imports awaited from an async export, each delayed 40 ms.
      assert.equal(await reference.octuple(3), 24);
      assert.ok(performance.now() - started >= 120, "every delayed result was awaited");
      reference.release();
    } finally {
      await host.close();
    }
  },
);
