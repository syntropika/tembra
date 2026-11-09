import { createServer } from "node:net";
import { once } from "node:events";
import { createRequire } from "node:module";
import { test } from "node:test";
import assert from "node:assert/strict";
import { tmpdir } from "node:os";
import path from "node:path";
import { readFile, mkdtemp, writeFile, rm } from "node:fs/promises";
import { createHost, directoryFilesystem } from "../dist/node.js";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;

test(
  "isolated TypeScript hooks compose waterfall, series and bail with a compiled contributor",
  { skip: !fixtures },
  async () => {
    const bytes = async (name) =>
      new Uint8Array(await readFile(path.join(fixtures, `${name}.wasm`)));
    const engine = process.env.TEMBRA_WASM_ENGINE;
    const host = createHost(
      engine ? { isolated: { engine: new Uint8Array(await readFile(engine)) } } : {},
    );
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
      await assert.rejects(reference.fail(), /Expected guest failure/);
      await assert.rejects(reference.forbidden());
      await assert.rejects(reference.malformed());
      assert.equal(await reference.fanout(), 24);
      await assert.rejects(reference.sum({ values: [0.5] }));
      assert.equal(await reference.recover(), 42);
      assert.deepEqual(await reference.checked(7), { tag: "ok", val: 7 });
      assert.deepEqual(await reference.checked(-1), { tag: "err", val: "negative" });
      await new Promise(setImmediate);
      for (const message of ["module log", "factory log", "method log"])
        assert(logs.some((record) => record.orb === "calculator" && record.message === message));
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
      const candidate = await host.prepare([replacement]);
      assert.equal(engines[0].instances(), 3);
      await candidate.abort();
      assert.equal(engines[0].instances(), 2);
      await host.reload([replacement]);
      assert.equal(await reference.read(), 24);
      await assert.rejects(reference.fail(), /Expected revised guest failure/);
      assert.equal(await reference["resource-value"](), 42);
      await assert.rejects(
        host.root(async (context) => {
          const pending = reference.pending(context);
          setTimeout(() => context.cancel(), 5);
          await pending;
        }),
      );
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
          grants: { network: [{ provider: "outbound", connect: [`127.0.0.1:${port}`] }] },
        });
        const network = await host.get("online::example:integrated/calculation@1.0.0", "main");
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
      await host.install([
        {
          artifact: await bytes("writer"),
          grants: { filesystem: [{ provider: "data", mount: "/data", writable: true }] },
        },
        {
          artifact: await bytes("reader"),
          grants: { filesystem: [{ provider: "data", mount: "/data", writable: false }] },
        },
      ]);
      const writer = await host.get("writer::example:storage/files@1.0.0", "main");
      const reader = await host.get("reader::example:storage/files@1.0.0", "main");
      assert.equal(await reader.read("/data/input.txt"), "initial");
      await writer.write("/data/output.txt", "persisted");
      assert.equal(await readFile(path.join(directory, "output.txt"), "utf8"), "persisted");
      await assert.rejects(reader.write("/data/output.txt", "forbidden"));
      await assert.rejects(reader.read("/data/../outside.txt"));
      const malicious = path.join(directory, "untrusted.mjs");
      await writeFile(malicious, "globalThis.__tembraUnsafeLoad = true; export default {};");
      await assert.rejects(host.load(malicious), /\.wasm artifact/);
      assert.equal(globalThis.__tembraUnsafeLoad, undefined);
      reader.release();
      writer.release();
      reference.release();
    } finally {
      try {
        await host.close();
      } finally {
        binding.NodeInterpreter = Original;
        await rm(directory, { recursive: true, force: true });
      }
    }
    for (const engine of engines) assert.equal(engine.instances(), 0);
    assert.equal(host.diagnostics().values, 0);
    assert.equal(host.diagnostics().pendingJobs, 0);
  },
);
