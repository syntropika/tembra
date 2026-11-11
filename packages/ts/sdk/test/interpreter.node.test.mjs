import { createServer } from "node:net";
import { once } from "node:events";
import { createRequire } from "node:module";
import { test } from "node:test";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import path from "node:path";
import { readFile, mkdtemp, writeFile, rm, readdir } from "node:fs/promises";
import { createHost, directoryFilesystem } from "../dist/node.js";
import { verifyPrecisionScenario } from "./precision-scenario.ts";
import { cleanup, closeQuarantined } from "./cleanup.ts";

/** Resolve once an attributed guest log record carries `message` (bounded to 10 s). */
async function logged(records, message) {
  const deadline = performance.now() + 10_000;
  while (!records.some((record) => record.message === message)) {
    if (performance.now() > deadline) throw new Error(`no guest log ${message}`);
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
}
import { verifyContextAcquireScenario } from "./context-acquire-scenario.ts";
import {
  COUNTERS as INGRESS_COUNTERS,
  INGRESS_ORB,
  replaced,
  request,
  settled,
  source,
  text,
  verifyIngressScenario,
} from "./ingress-scenario.ts";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;

test("Node: source Orbs select keyed instances per request", { skip: !fixtures }, async () => {
  const host = createHost();
  try {
    await verifyContextAcquireScenario(
      host,
      async (name) => new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`))),
    );
  } finally {
    await host.close();
  }
  assert.equal(host.diagnostics().references, 0);
  assert.equal(host.diagnostics().pendingJobs, 0);
});

test("Node: exact 64-bit values cross source Orbs without loss", { skip: !fixtures }, async () => {
  const host = createHost();
  try {
    const report = await verifyPrecisionScenario(
      host,
      async (name) => new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`))),
    );
    assert.match(report, /application -> source -> source/);
  } finally {
    await host.close();
  }
  for (const key of [
    "objects",
    "references",
    "calls",
    "roots",
    "resources",
    "tasks",
    "values",
    "pendingJobs",
  ])
    if (typeof host.diagnostics()[key] === "number") assert.equal(host.diagnostics()[key], 0, key);
});

test(
  "isolated TypeScript hooks compose waterfall, series and bail with a compiled contributor",
  { skip: !fixtures },
  async () => {
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    const host = createHost();
    const requires = Object.fromEntries(
      ["guard", "audit", "choice"].map((hook) => [
        `example:hooks/${hook}@1.0.0`,
        { orb: "tools", apiVersion: 1 },
      ]),
    );
    try {
      await host.install([
        { artifact: await bytes("hook-provider") },
        { artifact: await bytes("hook-extension"), requires },
        // The SDK revision sends this compiled port its contribute commands.
        { artifact: await bytes("hook-compiled"), requires },
      ]);
      const runner = await host.get("tools::example:hooks/runner@1.0.0", "main");
      const source = await host.get("extension::example:hooks/reporter@1.0.0", "main");
      const compiled = await host.get("hook-compiled::example:hooks/reporter@1.0.0", "main");
      // Waterfall in plan order: compiled (-7) adds 20, then source (0) adds 2 and blocks.
      assert.deepEqual(await runner.run({ value: 3, blocked: false }), {
        value: 25,
        blocked: true,
      });
      // Series: every participant observes each input; WIT void reaches consumers as null.
      for (const input of [5, 7]) assert.equal(await runner.audit(input), null);
      // Bail: `none` continues and the first `some` stops; an undecided `none` is null.
      assert.equal(await runner.choose(0), null);
      assert.equal(await runner.choose(1), 42);
      assert.equal(await runner.choose(2), 84);
      assert.deepEqual(await source.read(), { audit: 12, choices: 2 });
      assert.deepEqual(await compiled.read(), { audit: 12, choices: 3 });
      for (const reference of [runner, source, compiled]) reference.release();
    } finally {
      await host.close();
    }
    assert.equal(host.diagnostics().values, 0);
    assert.equal(host.diagnostics().pendingJobs, 0);
  },
);

test(
  "published Node host installs real TS and Rust Wasm artifacts with grants, reload and physical disposal",
  {
    skip: !fixtures,
    timeout: 180000,
  },
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-node-orb-"),
    );
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    const requirements = {
      "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 },
    };
    const logs = [];
    // Observe real native Store counts without adding a testing factory to the public SDK.
    const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");
    const Original = binding.NodeInterpreter;
    const engines = [];
    binding.NodeInterpreter = class extends Original {
      constructor(...args) {
        super(...args);
        engines.push(this);
      }
    };
    const options = {
      isolated: {
        providers: { network: ["outbound"], filesystem: { data: directoryFilesystem(directory) } },
        onLog(record) {
          logs.push(record);
          throw new Error("Observer failure");
        },
      },
    };
    const host = createHost(options);
    let failure;
    let stage = "install calculator";
    try {
      await writeFile(path.join(directory, "input.txt"), "initial");
      const provider = { artifact: await bytes("arithmetic") };
      const calculator = { artifact: await bytes("calculator"), requires: requirements };
      await host.install([provider, calculator]);
      assert.equal(engines[0].instances(), 2);
      const reference = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
      assert.equal(await reference.sum({ values: [2, 4, 6] }), 24);
      assert.equal(reference.state.value, 24);
      assert.equal(await reference["resource-value"](), 42);
      const installedVersion = host.diagnostics().codeVersion;
      await assert.rejects(host.install(calculator), /Use reload/);
      assert.equal(engines[0].instances(), 2);
      assert.equal(host.diagnostics().codeVersion, installedVersion);
      assert.equal(await reference.read(), 24);
      stage = "strict non-result traps";
      // An uncaught throw from a non-result WIT function traps and quarantines the revision;
      // reloading the same artifact restores service with the kernel's retained state.
      for (const failing of [
        // The trap does not carry the JavaScript error text; the quarantine below proves it.
        () => assert.rejects(reference.fail(), /component trap/),
        // The old private host seam no longer exists: probing it throws, which traps too.
        () => assert.rejects(reference.forbidden()),
        () => assert.rejects(reference.malformed()),
      ]) {
        await failing();
        await assert.rejects(reference.read(), /quarantined/);
        await host.reload([calculator]);
        assert.equal(await reference.read(), 24);
      }
      assert.equal(await reference.fanout(), 24);
      await assert.rejects(reference.sum({ values: [0.5] }));
      assert.equal(await reference.recover(), 42);
      assert.deepEqual(await reference.checked(7), { tag: "ok", val: 7 });
      assert.deepEqual(await reference.checked(-1), { tag: "err", val: "negative" });
      await new Promise(setImmediate);
      stage = "attributed logs";
      // Module-level console output is refused at build time (fixture-sources.test.ts).
      for (const message of ["factory log", "method log"])
        assert(logs.some((record) => record.orb === "calculator" && record.message === message));
      stage = "separate host";
      const other = createHost(options);
      try {
        await other.install([provider, calculator]);
        const separate = await other.get(
          "calculator::example:integrated/calculation@1.0.0",
          "main",
        );
        assert.equal(await separate.read(), 0);
        assert.equal(await separate.sum({ values: [1] }), 2);
        assert.equal(await reference.read(), 24);
        separate.release();
      } finally {
        await other.close();
      }
      assert.equal(engines[1].instances(), 0);
      assert.deepEqual(await Promise.all([reference.read(), reference.recover()]), [24, 42]);
      const replacement = { artifact: await bytes("revised"), requires: requirements };
      stage = "prepare and reload revised";
      const candidate = await host.prepare([replacement]);
      assert.equal(engines[0].instances(), 3);
      await candidate.abort();
      assert.equal(engines[0].instances(), 2);
      await host.reload([replacement]);
      assert.equal(await reference.read(), 24);
      await assert.rejects(reference.fail(), /component trap/);
      await assert.rejects(reference.read(), /quarantined/);
      await host.reload([replacement]);
      assert.equal(await reference["resource-value"](), 42);
      // Cancel once the guest entered its wait: the revoked wait escapes a non-result export,
      // so the revision fails closed; reload restores service.
      await assert.rejects(
        host.root(async (context) => {
          const pending = reference.pending(context);
          await logged(logs, "pending: waiting");
          context.cancel();
          await pending;
        }),
      );
      await assert.rejects(reference.recover(), /quarantined/);
      await host.reload([replacement]);
      assert.equal(await reference.recover(), 42);
      let releaseResponse;
      let enteredResponse;
      const entered = new Promise((resolve) => {
        enteredResponse = resolve;
      });
      const sockets = new Set();
      let observeSseClose;
      const sseClosed = new Promise((resolve) => {
        observeSseClose = resolve;
      });
      stage = "online HTTP";
      const server = createServer((socket) => {
        sockets.add(socket);
        socket.once("close", () => sockets.delete(socket));
        let request = "";
        let served = false;
        socket.on("data", (bytes) => {
          request += bytes.toString();
          if (served || !request.includes("\r\n\r\n")) return;
          served = true;
          if (request.startsWith("GET /events ")) {
            socket.once("close", observeSseClose);
            const response = Buffer.from(
              "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: hé🌍\n\n",
            );
            socket.write(response.subarray(0, response.length - 4));
            setImmediate(() => socket.write(response.subarray(response.length - 4)));
            // The peer stays open: the first event must settle before EOF and stop its reader.
            return;
          }
          const body = '{"values":[1,2,3]}';
          const respond = () =>
            socket.end(
              `HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${Buffer.byteLength(body)}\r\nConnection: close\r\n\r\n${body}`,
            );
          if (request.startsWith("GET /delayed ")) {
            releaseResponse = respond;
            enteredResponse();
          } else respond();
        });
      });
      server.listen(0, "127.0.0.1");
      await once(server, "listening");
      try {
        const { port } = server.address();
        const online = { artifact: await bytes("online"), requires: requirements };
        await assert.rejects(
          host.install(online),
          (error) => error.code === "forbidden" && error.reason === "orb_grant",
        );
        await host.install({
          ...online,
          grants: { http: [{ provider: "outbound", origins: [`http://127.0.0.1:${port}`] }] },
        });
        const network = await host.get("online::example:integrated/network@1.0.0", "main");
        assert.equal(await network["fetch-and-sum"](`http://127.0.0.1:${port}/values`), 12);
        assert.equal(await network["first-event"](`http://127.0.0.1:${port}/events`), "hé🌍");
        await sseClosed;
        const pending = network["fetch-and-sum"](`http://127.0.0.1:${port}/delayed`);
        await entered;
        let timer;
        try {
          // Even with UV_THREADPOOL_SIZE=1, Orb sockets must not block Node filesystem work.
          const text = await Promise.race([
            readFile(path.join(directory, "input.txt"), "utf8"),
            new Promise((_, reject) => {
              timer = setTimeout(
                () => reject(new Error("Orb I/O blocked Node's worker pool")),
                1000,
              );
            }),
          ]);
          assert.equal(text, "initial");
        } finally {
          clearTimeout(timer);
          releaseResponse();
          assert.equal(await pending, 12);
        }
        network.release();
      } finally {
        for (const socket of sockets) socket.destroy();
        await new Promise((resolve, reject) =>
          server.close((error) => (error ? reject(error) : resolve())),
        );
      }
      stage = "storage grants";
      await host.install([
        {
          artifact: await bytes("writer"),
          grants: {
            filesystem: [{ provider: "data", mount: "/data", writable: true }],
          },
        },
        {
          artifact: await bytes("reader"),
          grants: {
            filesystem: [{ provider: "data", mount: "/data", writable: false }],
          },
        },
      ]);
      const writer = await host.get("writer::example:storage/files@1.0.0", "main");
      const reader = await host.get("reader::example:storage/files@1.0.0", "main");
      assert.equal(await reader.read("/data/input.txt"), "initial");
      await writer.write("/data/output.txt", "persisted");
      assert.equal(await readFile(path.join(directory, "output.txt"), "utf8"), "persisted");
      // `files` has no failure lane: each refusal traps and fails its revision closed, so each
      // strict case runs on a fresh reader revision.
      const readerDefinition = {
        artifact: await bytes("reader"),
        grants: { filesystem: [{ provider: "data", mount: "/data", writable: false }] },
      };
      stage = "read-only write refused";
      await assert.rejects(reader.write("/data/output.txt", "forbidden"), /component trap/);
      await assert.rejects(reader.read("/data/input.txt"), /quarantined/);
      assert.equal(await readFile(path.join(directory, "output.txt"), "utf8"), "persisted");
      await host.reload([readerDefinition]);
      stage = "traversal refused";
      await assert.rejects(reader.read("/data/../outside.txt"), /component trap/);
      await host.reload([readerDefinition]);
      assert.equal(await reader.read("/data/input.txt"), "initial");
      stage = "artifact loading";
      const malicious = path.join(directory, "untrusted.mjs");
      await writeFile(malicious, "globalThis.__tembraUnsafeLoad = true; export default {};");
      await assert.rejects(host.load(malicious), /\.wasm artifact/);
      assert.equal(globalThis.__tembraUnsafeLoad, undefined);
      reader.release();
      writer.release();
      reference.release();
    } catch (error) {
      failure = new Error(`${stage}: ${error?.message ?? error}`, { cause: error });
    }
    // The strict `fail`/`forbidden` cases quarantined calculator revisions that held source
    // resources: their cleanup failure is expected and asserted. Otherwise cleanup reports its
    // own failure together with the test's; neither is masked.
    try {
      if (failure) await cleanup(failure, () => host.close());
      else await closeQuarantined(() => host.close());
    } finally {
      binding.NodeInterpreter = Original;
      await rm(directory, { recursive: true, force: true });
    }
    if (failure) throw failure;
    for (const engine of engines) assert.equal(engine.instances(), 0);
    assert.equal(host.diagnostics().values, 0);
    assert.equal(host.diagnostics().pendingJobs, 0);
  },
);

test(
  "cancelling a caller interrupts a pending source HTTP send: a result lane recovers, a strict callback fails closed",
  { skip: !fixtures, timeout: 60000 },
  async () => {
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    const sockets = new Set();
    /** One stalled exchange: the peer's close and the request's arrival. */
    function fresh() {
      const state = {};
      state.closed = new Promise((resolve) => {
        state.peerClosed = resolve;
      });
      state.waiting = new Promise((resolve) => {
        state.entered = resolve;
      });
      return state;
    }
    let { closed, peerClosed, waiting, entered } = fresh();
    // The /stalled peer reads the request and never answers; /values answers at once.
    const server = createServer((socket) => {
      sockets.add(socket);
      socket.once("close", () => sockets.delete(socket));
      let request = "";
      socket.on("data", (chunk) => {
        request += chunk.toString();
        if (!request.includes("\r\n\r\n")) return;
        if (request.startsWith("GET /stalled ")) {
          socket.once("close", () => peerClosed(performance.now()));
          entered();
          return;
        }
        const body = '{"values":[1,2,3]}';
        socket.end(
          `HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: ${body.length}\r\nConnection: close\r\n\r\n${body}`,
        );
      });
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const { port } = server.address();
    const origin = `http://127.0.0.1:${port}`;
    /** Install online on a fresh host, cancel a stalled `method` call and check the peer closes. */
    async function cancelled(method, after) {
      let peer = closed;
      const host = createHost({ isolated: { providers: { network: ["outbound"] } } });
      try {
        await host.install([
          { artifact: await bytes("arithmetic") },
          {
            artifact: await bytes("online"),
            requires: {
              "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 },
            },
            grants: { http: [{ provider: "outbound", origins: [origin] }] },
          },
        ]);
        const network = await host.get("online::example:integrated/network@1.0.0", "main");
        let cancelledAt;
        const started = performance.now();
        await assert.rejects(
          host.root(async (context) => {
            const pending = network[method](`${origin}/stalled`, context);
            await waiting;
            setTimeout(() => {
              cancelledAt = performance.now();
              context.cancel();
            }, 100);
            return await pending;
          }),
        );
        // The response head never arrives, so only cancellation can close the connection; the
        // per-operation deadline is 10 s.
        let timer;
        const closedAt = await Promise.race([
          peer,
          new Promise((_, reject) => {
            timer = setTimeout(
              () => reject(new Error("the pending send outlived its caller")),
              2000,
            );
          }),
        ]).finally(() => clearTimeout(timer));
        assert.ok(closedAt - cancelledAt < 2000, `peer closed ${closedAt - cancelledAt} ms late`);
        await after(network);
        assert.ok(performance.now() - started < 5000, "the Orb waited for the I/O deadline");
        network.release();
      } finally {
        await host.close();
      }
    }
    try {
      // A declared `result` lane makes the revoked send recoverable: the same Store serves the
      // next callback, neither quarantined nor left busy.
      await cancelled("fetch-and-sum-checked", async (network) =>
        assert.deepEqual(await network["fetch-and-sum-checked"](`${origin}/values`), {
          tag: "ok",
          val: 12,
        }),
      );
      // Without a failure lane the uncaught failure traps and the revision fails closed.
      ({ closed, peerClosed, waiting, entered } = fresh());
      await cancelled("fetch-and-sum", (network) =>
        assert.rejects(network["fetch-and-sum"](`${origin}/values`), /quarantined/),
      );
    } finally {
      for (const socket of sockets) socket.destroy();
      server.close();
    }
  },
);

test(
  "standard WASI source Orbs read raw bytes, retain descriptors and refuse forged handles natively",
  { skip: !fixtures, timeout: 60000 },
  async () => {
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    const data = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-standard-"),
    );
    await writeFile(
      path.join(data, "bytes.bin"),
      Uint8Array.from({ length: 4100 }, (_, index) => index % 256),
    );
    await writeFile(
      path.join(data, "large.bin"),
      Uint8Array.from({ length: 3 * (1 << 20) + 5 }, (_, index) => index % 251),
    );
    const standardLogs = [];
    const host = createHost({
      isolated: {
        providers: { filesystem: { data: directoryFilesystem(data) } },
        onLog: (record) => void standardLogs.push(record),
      },
    });
    const filesystem = [{ provider: "data", mount: "/data", writable: false }];
    const grants = { system: ["clocks", "random"], filesystem };
    const probing = { filesystem };
    const [standard, boundary, peer] = await Promise.all(
      ["standard", "boundary", "boundary-peer"].map(bytes),
    );
    try {
      // Imports without matching grants refuse installation.
      await assert.rejects(host.install({ artifact: standard, grants: { filesystem } }));
      await assert.rejects(host.install({ artifact: standard, grants: { system: grants.system } }));
      await host.install([
        { artifact: standard, grants },
        { artifact: boundary, grants: probing },
        { artifact: peer, grants: probing },
      ]);
      const scenario = await host.get("standard::example:standard/scenario@1.0.0", "main");
      assert.deepEqual(JSON.parse(await scenario.probe()), {
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
      });
      const reused = JSON.stringify({ size: "4100", tail: [0, 1, 2, 3] });
      assert.equal(await scenario.reuse(), reused);
      const large = JSON.parse(await scenario.large());
      assert.equal(large.length, 3 * (1 << 20) + 5);
      assert.ok(large.chunks > 1 && large.largest <= 65536, JSON.stringify(large));
      assert.deepEqual([large.matches, large.done], [true, "ok"]);
      // Cancelling `hang` (a non-result callback) once it waits in the standard clock settles
      // the call promptly.
      const cancelledAt = performance.now();
      await assert.rejects(
        host.root(async (context) => {
          const pending = scenario.hang(context);
          await logged(standardLogs, "standard:waiting");
          context.cancel();
          await pending;
        }),
      );
      assert.ok(performance.now() - cancelledAt < 5000, "the cancelled wait settled promptly");
      // Measured on Node: the host tears down the cancelled callback while it is blocked in the
      // WASI wait, so no guest code observes the revocation; the revision is not quarantined
      // and the retained descriptor survives (native `hold` keeps the same invariant).
      assert.equal(await scenario.reuse(), reused);
      // Finite detached work (40 ms wait + stat) belongs to the callback: it completes
      // physically before the call settles, and the descriptor stays usable.
      const abandoned = performance.now();
      assert.equal(await scenario.abandon(), "returned");
      assert.ok(performance.now() - abandoned >= 30, "detached work finished within the call");
      assert.equal(await scenario.reuse(), reused);
      assert.deepEqual(JSON.parse(await scenario.denied()), {
        traversal: "not-permitted",
        write: "not-permitted",
        create: "not-permitted",
        missing: "no-entry",
      });
      const probes = await host.get("boundary::example:standard/probes@1.0.0", "main");
      const foreign = await host.get("boundary-peer::example:standard/probes@1.0.0", "main");
      // No old private seam exists, and the runtime refuses a disposed descriptor.
      const boundaryProbes = JSON.parse(await probes.probes());
      assert.deepEqual(boundaryProbes.seams, []);
      assert.equal(typeof boundaryProbes.droppedObject, "string");
      assert.notEqual(boundaryProbes.droppedObject, "accepted");
      // Descriptor indices name only the Orb's own retained descriptors.
      assert.equal(await probes.descriptor(), 0);
      assert.equal(await probes.inspect(0), "accepted");
      assert.equal(await foreign.descriptor(), 0);
      assert.equal(await foreign.descriptor(), 1);
      assert.equal(await probes.inspect(1), "unknown descriptor");
      assert.equal(await foreign.inspect(1), "accepted");
      // A reloaded revision starts without the retired revision's descriptor; `reuse` has no
      // failure lane, so its refusal traps (the trap carries no JavaScript text).
      await host.reload([{ artifact: standard, grants }]);
      await assert.rejects(scenario.reuse(), /component trap/);
      for (const reference of [scenario, probes, foreign]) reference.release();
    } finally {
      await host.close();
      await rm(data, { recursive: true, force: true });
    }
  },
);

test(
  "TypeScript uses the complete standard filesystem path for binary writes and directory operations",
  { skip: !fixtures, timeout: 60000 },
  async () => {
    const data = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-filesystem-"),
    );
    const artifact = new Uint8Array(await readFile(path.join(fixtures, "filesystem.wasm")));
    const host = createHost({
      isolated: {
        providers: { filesystem: { data: directoryFilesystem(data) } },
        // This scenario also verifies every byte in guest JavaScript; retain the normal defaults elsewhere.
        limits: { fuel: 500_000_000 },
      },
    });
    const grants = (writable) => ({
      filesystem: [{ provider: "data", mount: "/data", writable }],
    });
    try {
      await host.install({ artifact, grants: grants(false) });
      const readonly = await host.get("filesystem::example:filesystem/scenario@1.0.0", "main");
      // A refused WASI call reaches the declared failure lane as its error-code tag.
      assert.deepEqual(await readonly["roundtrip-checked"](), { tag: "err", val: "not-permitted" });
      assert.deepEqual(await readdir(data), []);
      readonly.release();
      await host.reload([{ artifact, grants: grants(true) }]);
      const reference = await host.get("filesystem::example:filesystem/scenario@1.0.0", "main");
      try {
        assert.deepEqual(JSON.parse(await reference.roundtrip()), {
          length: 150006,
          listing: ["renamed.bin"],
          times: "ok",
          links: "ok",
          symlinks: "ok",
          directoryRename: "moved",
        });
      } finally {
        reference.release();
      }
      assert.deepEqual(await readdir(data), []);
    } finally {
      await host.close();
      await rm(data, { recursive: true, force: true });
    }
    assert.equal(host.diagnostics().resources, 0);
    assert.equal(host.diagnostics().values, 0);
  },
);

test(
  "standard clocks, random, CLI and builtin authority run through actual source Wasm",
  { skip: !fixtures },
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-cli-"),
    );
    const output = path.join(directory, "stdout.bin"),
      error = path.join(directory, "stderr.bin");
    const input = Uint8Array.from({ length: 131075 }, (_, index) => index % 251);
    const host = createHost();
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    try {
      await host.install({ artifact: await bytes("ambient") });
      const policy = await host.get("ambient::example:system/policy@1.0.0", "main");
      // Without grants, ambient time is frozen at the epoch and randomness has a fixed seed: the
      // Orb observes neither host time nor host entropy.
      const ambient = JSON.parse(await policy.inspect());
      assert.equal(ambient.now, 0);
      assert.equal(ambient.date, 0);
      assert.equal(ambient.constructor, 0);
      assert.equal(Date.parse(ambient.call), 0);
      assert.equal(ambient.pure, "1970-01-01T00:00:00.000Z");
      assert.ok(Number.isFinite(ambient.random) && ambient.random >= 0 && ambient.random < 1);
      policy.release();
      const again = createHost();
      try {
        await again.install({ artifact: await bytes("ambient") });
        const repeated = await again.get("ambient::example:system/policy@1.0.0", "main");
        assert.equal(JSON.parse(await repeated.inspect()).random, ambient.random);
        repeated.release();
      } finally {
        await again.close();
      }
      await assert.rejects(
        host.install({ artifact: await bytes("system"), grants: { system: ["clocks", "random"] } }),
      );
      await host.install({
        artifact: await bytes("system"),
        grants: { system: ["clocks", "random", "cli"] },
        cli: {
          arguments: ["probe", "--bytes"],
          environment: [["MODE", "test"]],
          initialCwd: "/data",
          stdin: { bytes: input },
          stdout: { file: output },
          stderr: { file: error },
        },
      });
      const scenario = await host.get("system::example:system/scenario@1.0.0", "main");
      try {
        const result = JSON.parse(await scenario.inspect());
        assert.deepEqual(result.arguments, ["probe", "--bytes"]);
        assert.deepEqual(result.environment, [["MODE", "test"]]);
        assert.equal(result.cwd, "/data");
        assert.equal(result.secureLength, 16);
        assert.equal(result.insecureLength, 17);
        assert.equal(result.seedStable, true);
        assert.equal(result.terminal, true);
        assert.ok(result.date > 1_600_000_000_000);
        assert.ok(result.random >= 0 && result.random < 1);
        assert.deepEqual(JSON.parse(await scenario.transfer()), {
          length: input.length,
          first: 0,
          last: input[input.length - 1],
        });
        assert.deepEqual(new Uint8Array(await readFile(output)), input);
        assert.deepEqual(new Uint8Array(await readFile(error)), new Uint8Array([0, 255, 128, 10]));
        await assert.rejects(scenario.stop(7), /orb exited with status 7/);
        await assert.rejects(scenario.inspect());
      } finally {
        scenario.release();
      }
    } finally {
      await host.close();
      await rm(directory, { recursive: true, force: true });
    }
    assert.equal(host.diagnostics().values, 0);
    assert.equal(host.diagnostics().pendingJobs, 0);
  },
);

/** Count actual native Stores of every interpreter backend the SDK creates during `run`. */
async function countingStores(run) {
  const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");
  const Original = binding.NodeInterpreter;
  const engines = [];
  binding.NodeInterpreter = class extends Original {
    constructor(...args) {
      super(...args);
      engines.push(this);
    }
  };
  try {
    return { result: await run(), engines };
  } finally {
    binding.NodeInterpreter = Original;
  }
}

/** Every kernel counter and every native Store is gone once the host closed. */
function assertRetired(host, engines) {
  const diagnostics = host.diagnostics();
  for (const key of INGRESS_COUNTERS) assert.equal(diagnostics[key], 0, key);
  assert.ok(engines.length > 0, "the native carrier was used");
  for (const engine of engines) assert.equal(engine.instances(), 0, "native Stores");
}

const ingressHost = async (limits) => {
  return createHost({
    isolated: {
      ...(limits ? { limits } : {}),
    },
  });
};

test(
  "Node: Host.handleHttp serves a source handler through the native typed carrier",
  { skip: !fixtures, timeout: 120000 },
  async () => {
    const hosts = [];
    const { result, engines } = await countingStores(() =>
      verifyIngressScenario({
        open: async (limits) => {
          const host = await ingressHost(limits);
          hosts.push(host);
          return host;
        },
        fixture: async (name) =>
          new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`))),
        // Node's WHATWG Response keeps set-cookie; browsers hide it.
        setCookie: "kept",
      }),
    );
    assert.match(result, /reload drains the in-flight stream/);
    for (const host of hosts) assertRetired(host, engines);
  },
);

test(
  "Node: native ingress never waits on the application's body and fails broken bodies",
  { skip: !fixtures, timeout: 60000 },
  async () => {
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    let host;
    const { engines } = await countingStores(async () => {
      host = await ingressHost();
      try {
        await host.install({ artifact: await bytes("ledger") });
        await host.install({
          artifact: await bytes("ingress"),
          requires: { "example:ingress/ledger@1.0.0": { orb: "ledger", apiVersion: 1 } },
        });
        // An application body whose next() and return() never settle: a disconnect still ends
        // the call; nothing waits on the orphaned promises.
        const orphan = { nexts: 0, returns: 0 };
        const stalled = {
          [Symbol.asyncIterator]: () => ({
            next: () => (orphan.nexts++, new Promise(() => {})),
            return: () => (orphan.returns++, new Promise(() => {})),
          }),
        };
        const idle = await host.handleHttp(INGRESS_ORB, request("/idle", stalled));
        const events = idle.body[Symbol.asyncIterator]();
        await events.next();
        // Disconnect only once the guest's body read is pending on the application's next(),
        // so the case proves a never-settling read does not hold the call (bounded wait).
        for (let tries = 0; orphan.nexts === 0 && tries < 500; tries++)
          await new Promise((resolve) => setTimeout(resolve, 10));
        await events.return();
        await settled(host);
        assert.deepEqual(orphan, { nexts: 1, returns: 1 });

        // A handler that never reads its body: the body is returned once when the call ends.
        const unread = source();
        assert.match(
          await text(await host.handleHttp(INGRESS_ORB, request("/revision", unread.body))),
          /^\d+$/,
        );
        await settled(host);
        assert.deepEqual(unread.state, { returned: 1, reads: 0 });

        // A consumer that drops the body before reading it is a disconnect.
        const dropped = source();
        const early = await host.handleHttp(INGRESS_ORB, request("/sse", dropped.body));
        await early.body[Symbol.asyncIterator]().return();
        await settled(host);
        assert.equal(dropped.state.returned, 1);

        // A failing request body (a throwing iterator, a non-byte chunk, rejected or invalid
        // trailers) reaches the handler as a body error; its echo fails, never ends truncated.
        for (const [what, body, trailers] of [
          [
            "throwing iterator",
            () =>
              (async function* () {
                yield Uint8Array.of(1, 2, 3);
                throw new Error("upload failed");
              })(),
          ],
          [
            "non-byte chunk",
            () =>
              (async function* () {
                yield [1, 2, 3];
              })(),
          ],
          ["rejected trailers", () => null, () => Promise.reject(new Error("trailers failed"))],
          // HTTP field values exclude control bytes: native validation fails the guest's body.
          [
            "invalid trailer bytes",
            () => null,
            () => Promise.resolve([["x-bad", Uint8Array.of(0)]]),
          ],
        ]) {
          // Created only now, so no rejection exists before the host observes it.
          const extra = trailers ? { trailers: trailers() } : {};
          const echo = await host.handleHttp(INGRESS_ORB, request("/echo", body(), extra));
          const outcome = await text(echo).then(
            () => "ended",
            (error) => `failed: ${error.name}`,
          );
          assert.equal(outcome, "failed: HttpHandlerError", what);
          await assert.rejects(echo.trailers, what);
          await settled(host);
        }

        // A head the native carrier refuses before admission fails the call and returns the body.
        const refusedBody = source();
        await assert.rejects(
          host.handleHttp(INGRESS_ORB, request("/sse", refusedBody.body, { method: "BAD METHOD" })),
          /method/,
        );
        await settled(host);
        await replaced(host, INGRESS_ORB);
        await assert.rejects(
          host.handleHttp(INGRESS_ORB, request("/sse", null, { authority: undefined })),
          /authority/,
        );
        await settled(host);
        await replaced(host, INGRESS_ORB);
        assert.equal(refusedBody.state.returned, 1);
        // Handler error-codes with payloads arrive as exact WIT values (typed bytes, no text).
        await assertTypedRefusals(host, INGRESS_ORB);
        // The revision still serves after every failure above.
        assert.match(await text(await host.handleHttp(INGRESS_ORB, request("/revision"))), /^\d+$/);
      } finally {
        await host.close();
      }
    });
    assertRetired(host, engines);
  },
);

test(
  "Node: a request body that cannot be opened never strands the native call (M-1, L-1)",
  { skip: !fixtures, timeout: 60000 },
  async () => {
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    const unhandled = [];
    const record = (reason) => unhandled.push(reason);
    process.on("unhandledRejection", record);
    let host;
    try {
      const { engines } = await countingStores(async () => {
        host = await ingressHost();
        try {
          await host.install({ artifact: await bytes("ledger") });
          await host.install({
            artifact: await bytes("ingress"),
            requires: { "example:ingress/ledger@1.0.0": { orb: "ledger", apiVersion: 1 } },
          });
          // A locked stream throws from its iterator factory (`getReader()`), like a body that
          // middleware already consumed.
          const locked = () => {
            const stream = new ReadableStream();
            stream.getReader();
            return {
              [Symbol.asyncIterator]: () => {
                stream.getReader();
              },
            };
          };
          // Unread completion: the call finishes and the revision serves the next request.
          const empty = await host.handleHttp(INGRESS_ORB, request("/status/204", locked()));
          assert.equal(empty.status, 204);
          assert.equal((await empty.body[Symbol.asyncIterator]().next()).done, true);
          await settled(host);
          // A head refused before admission reports its own refusal.
          await assert.rejects(
            host.handleHttp(INGRESS_ORB, request("/sse", locked(), { method: "BAD METHOD" })),
            /method/,
          );
          await settled(host);
          await replaced(host, INGRESS_ORB);
          // A consumer disconnect still reaches the writing guest.
          const writing = await host.handleHttp(INGRESS_ORB, request("/write", locked()));
          const events = writing.body[Symbol.asyncIterator]();
          await events.next();
          assert.deepEqual(await events.return(), { done: true, value: undefined });
          await settled(host);
          // The realistic WHATWG trigger: a Request whose body middleware already read.
          const consumed = new Request("https://app.example.test/status/204", {
            method: "POST",
            body: "already read",
          });
          await consumed.text();
          assert.equal((await host.handleHttp(INGRESS_ORB, consumed)).status, 204);
          await settled(host);
          // L-1: a call failing before any port took the request still closes its body once.
          const body = source();
          await assert.rejects(host.handleHttp("missing", request("/", body.body)));
          await settled(host);
          assert.equal(body.state.returned, 1);
          await assert.rejects(host.handleHttp("missing", request("/", locked())));
          await settled(host);
          // Nothing was stranded: the revision still serves.
          assert.match(
            await text(await host.handleHttp(INGRESS_ORB, request("/revision"))),
            /^\d+$/,
          );
        } finally {
          await host.close();
        }
      });
      assertRetired(host, engines);
      await new Promise((resolve) => setTimeout(resolve, 20));
      assert.deepEqual(unhandled, []);
    } finally {
      process.off("unhandledRejection", record);
    }
  },
);

/** The fixtures' payload refusals (source `orb.ts` and compiled `rust-http-handler` alike). */
const REFUSALS = [
  ["/refuse/escaped", { tag: "internal-error", val: 'quote " slash \\ line\nend é🌍' }],
  [
    "/refuse/record",
    { tag: "HTTP-request-header-size", val: { fieldName: "x-big", fieldSize: 4294967295 } },
  ],
  ["/refuse/u64", { tag: "HTTP-response-body-size", val: 18446744073709551615n }],
  ["/refuse/dns", { tag: "DNS-error", val: { rcode: "SERVFAIL", infoCode: 65535 } }],
];

/** Each refusal is an `HttpHandlerError` carrying exactly the guest's WIT `error-code`. */
async function assertTypedRefusals(host, orb, probe) {
  for (const [route, code] of REFUSALS) {
    const refused = await host.handleHttp(orb, request(route)).catch((error) => error);
    assert.equal(refused?.name, "HttpHandlerError", `${route}: ${String(refused)}`);
    assert.deepStrictEqual(refused.code, code, route);
    await settled(host);
    await replaced(host, orb, probe);
  }
}

// The compiled Rust handler (`rust-http-handler`, raw component) behind the same SDK facade.
const httpHandler = process.env.TEMBRA_WASM_HTTP_HANDLER;
const handlerWit = path.resolve(
  import.meta.dirname,
  "../../../../crates/executor-wasm/tests/wit/handler",
);

test(
  "Node: Host.handleHttp serves a compiled handler through the native typed carrier",
  { skip: !httpHandler, timeout: 60000 },
  async () => {
    const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");
    const artifact = new Uint8Array(
      new binding.NodeContract(handlerWit, "handler").componentArtifact(
        new Uint8Array(await readFile(httpHandler)),
        "web",
        1,
      ),
    );
    const decoder = new TextDecoder();
    const hosts = [];
    const { engines } = await countingStores(async () => {
      const host = await ingressHost();
      hosts.push(host);
      try {
        await host.install({ artifact });
        const status = async () => {
          const response = await host.handleHttp("web", request("/status"));
          assert.equal((await response.body[Symbol.asyncIterator]().next()).done, true);
          return Object.fromEntries(
            response.fields.map(([name, value]) => [name, decoder.decode(value)]),
          );
        };

        // Head and first event before the request EOF; raw binary chunks streamed one by one.
        const body = source();
        const sse = await host.handleHttp(
          "web",
          request("/sse", body.body, {
            fields: [
              ["x-raw", Uint8Array.of(0xff, 0x41, 0x80)],
              ["x-raw", Uint8Array.of(0x21)],
            ],
          }),
        );
        assert.equal(sse.status, 200);
        const events = sse.body[Symbol.asyncIterator]();
        assert.equal(decoder.decode((await events.next()).value), "data: open\n\n");
        body.push(new TextEncoder().encode("alpha"));
        assert.equal(decoder.decode((await events.next()).value), "data: alpha\n\n");
        body.push(Uint8Array.of(0x62, 0x65, 0x74, 0x61));
        assert.equal(decoder.decode((await events.next()).value), "data: beta\n\n");
        body.end();
        assert.equal(decoder.decode((await events.next()).value), "data: end\n\n");
        assert.equal((await events.next()).done, true);
        assert.deepEqual(
          (await sse.trailers).map(([name, value]) => [name, decoder.decode(value)]),
          [["x-events", "4"]],
        );
        await settled(host);
        assert.match((await status())["x-last-exit"], /^complete events=4 transmit=Ok/);

        // Its error-code and trap are reported after settlement; the Store keeps serving.
        const refused = await host.handleHttp("web", request("/error")).catch((error) => error);
        assert.equal(refused.name, "HttpHandlerError", String(refused));
        assert.deepEqual(refused.code, { tag: "internal-error", val: "refused" });
        await settled(host);
        await replaced(host, "web", "/status");
        await assertTypedRefusals(host, "web", "/status");

        // A consumer disconnect while the producer writes, and cancellation through a signal.
        const stream = await host.handleHttp("web", request("/stream"));
        const streamEvents = stream.body[Symbol.asyncIterator]();
        await streamEvents.next();
        await streamEvents.return();
        await settled(host);
        const cancel = new AbortController();
        const cancelled = await host.handleHttp("web", request("/stream"), {
          signal: cancel.signal,
        });
        const cancelledEvents = cancelled.body[Symbol.asyncIterator]();
        await cancelledEvents.next();
        cancel.abort();
        await assert.rejects(async () => {
          for (;;) if ((await cancelledEvents.next()).done) break;
        });
        await assert.rejects(cancelled.trailers);
        await settled(host);
        assert.match((await status())["x-requests"], /^\d+$/);

        // A trap is terminal for the compiled revision: reported after settlement, then every
        // later request is refused as quarantined until a reload.
        await assert.rejects(host.handleHttp("web", request("/trap")), /trap/);
        await settled(host);
        await assert.rejects(host.handleHttp("web", request("/status")), (error) =>
          /quarantined|closing/.test(`${error.reason} ${error.message}`),
        );
        await settled(host);
      } finally {
        await host.close();
      }

      // A head that never comes: the revision timeout fails the call before any head.
      const limited = await ingressHost({ timeoutMs: 1000 });
      hosts.push(limited);
      try {
        await limited.install({ artifact });
        const stalled = source();
        await assert.rejects(
          limited.handleHttp("web", request("/slow", stalled.body)),
          /timed out|wall time|quarantin/i,
        );
        await settled(limited);
        assert.equal(stalled.state.returned, 1);
      } finally {
        await limited.close();
      }
    });
    for (const host of hosts) assertRetired(host, engines);
  },
);
