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
const service = "example:probe/probe@1.0.0";
const contract = path.resolve(
  import.meta.dirname,
  "../../../../crates/executor-wasm/tests/wit/probe",
);
const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");

/** Mirror the guest's wrapping u32 checksum. */
function checksum(rounds) {
  let value = 0;
  for (let round = 0; round < rounds; round += 1) value = (Math.imul(value, 31) + round) >>> 0;
  return value;
}

/** Package the raw component as one closed Tembra artifact. */
async function artifact(name) {
  const component = new Uint8Array(await readFile(probe));
  const resolved = new binding.NodeContract(
    binding.compileContract(contract, "probe-provider"),
    "probe-provider",
  );
  return new Uint8Array(resolved.artifact(undefined, component, name, 1));
}

/** Open a component port directly on the native transport with one read-only data grant. */
function open(interpreter, bytes, name) {
  const artifact = new binding.NodeArtifact(bytes);
  const grants = { filesystem: [{ provider: "data", mount: "/data", writable: false }] };
  const plan = JSON.parse(
    artifact.request(
      JSON.stringify({
        kind: "install",
        name,
        apiVersion: 1,
        requires: {},
        grants,
        filesystem: ["data"],
        network: [],
      }),
    ),
  );
  assert(Object.hasOwn(plan, "ok"), JSON.stringify(plan));
  const configuration = JSON.stringify({ bindings: plan.ok.bindings, grants });
  return interpreter.openComponent(artifact, configuration);
}

/** Decode one native call envelope. */
function reply(packet) {
  return JSON.parse(packet);
}

test(
  "compiled component calls wait on their worker, not on Node's libuv pool",
  { skip: !probe || process.platform === "win32", timeout: 60000 },
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-node-component-"),
    );
    const fifo = path.join(directory, "pipe");
    let release;
    try {
      await writeFile(path.join(directory, "input.txt"), "initial");
      assert.equal(spawnSync("mkfifo", [fifo]).status, 0);
      const bytes = await artifact("probe");
      const interpreter = new binding.NodeInterpreter(
        JSON.stringify({ filesystem: { data: directory } }),
        () => {},
      );
      const port = open(interpreter, bytes, "probe");
      assert.equal(interpreter.instances(), 1);
      // Refused input and unknown methods settle without poisoning the Store.
      const refused = reply(await port.call(service, "spin", JSON.stringify(["x"])));
      assert.equal(typeof refused.error.message, "string");
      const unknown = reply(await port.call(service, "missing", "[]"));
      assert.match(unknown.error.message, /unknown WIT export/);
      assert.deepEqual(reply(await port.call(service, "spin", "[3]")), { ok: checksum(3) });

      // Occupy more calls than libuv has threads; the first blocks inside a WASI open.
      const threads = Number(process.env.UV_THREADPOOL_SIZE) || 4;
      let settled = 0;
      const track = (promise) => promise.finally(() => (settled += 1));
      const blocked = track(port.call(service, "open", JSON.stringify(["/data/pipe"])));
      const queued = Array.from({ length: threads }, (_, index) =>
        track(port.call(service, "spin", JSON.stringify([1000 + index]))),
      );
      // The observer releases its claim; submitted calls keep the Store alive.
      port.close();
      assert.throws(() => port.call(service, "spin", "[1]"), /component is closed/);
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
        assert.equal(interpreter.instances(), 1);
      } finally {
        clearTimeout(timer);
        // A child writer opens the FIFO without using the libuv pool.
        release = spawn("sh", ["-c", ': > "$1"', "release", fifo], { stdio: "ignore" });
      }
      assert.deepEqual(reply(await blocked), { ok: { tag: "ok" } });
      const results = await Promise.all(queued);
      results.forEach((packet, index) =>
        assert.deepEqual(reply(packet), { ok: checksum(1000 + index) }),
      );
      // The last settled call released the closed port and joined its idle worker.
      assert.equal(interpreter.instances(), 0);
    } finally {
      if (release && release.exitCode === null) {
        const exited = once(release, "exit");
        const kill = setTimeout(() => release.kill(), 5000);
        await exited;
        clearTimeout(kill);
      }
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "a terminal component failure settles every queued call with the quarantine",
  { skip: !probe, timeout: 60000 },
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-node-component-"),
    );
    try {
      const interpreter = new binding.NodeInterpreter(
        JSON.stringify({
          limits: { timeoutMs: 200, fuel: 2 ** 40 },
          filesystem: { data: directory },
        }),
        () => {},
      );
      const port = open(interpreter, await artifact("busy"), "busy");
      const calls = [
        port.call(service, "spin", JSON.stringify([4294967295])),
        port.call(service, "spin", "[1]"),
        port.call(service, "spin", "[2]"),
      ];
      for (const packet of await Promise.all(calls))
        assert.match(reply(packet).error.message, /timed out/);
      assert.match(reply(await port.call(service, "spin", "[1]")).error.message, /timed out/);
      // The failed Store is gone before its failure was published.
      assert.equal(interpreter.instances(), 0);
      port.close();
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
  },
);

test(
  "the published Node host installs the compiled probe with a filesystem grant",
  { skip: !probe, timeout: 60000 },
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-node-component-"),
    );
    const host = createHost({
      isolated: { providers: { filesystem: { data: directoryFilesystem(directory) } } },
    });
    try {
      await writeFile(path.join(directory, "input.txt"), "initial");
      await host.install({
        artifact: await artifact("probe"),
        grants: { filesystem: [{ provider: "data", mount: "/data", writable: true }] },
      });
      const reference = await host.get(`probe::${service}`, "main");
      assert.equal(await reference.spin(1000), checksum(1000));
      assert.equal((await reference.open("/data/input.txt")).tag, "ok");
      assert.equal((await reference["create-directory"]("/data/made")).tag, "ok");
      assert.equal((await reference.open("/data/../outside.txt")).tag, "err");
      reference.release();
    } finally {
      try {
        await host.close();
      } finally {
        await rm(directory, { recursive: true, force: true });
      }
    }
  },
);
