import { createServer } from "node:http";
import { once } from "node:events";
import { test } from "node:test";
import assert from "node:assert/strict";
import { createHost as createGenericHost } from "../dist/host.js";
import { NodeKernel, compileContract } from "@syntropika/tembra-engine-node";
import { tmpdir } from "node:os";
import path from "node:path";
import { readFile, mkdtemp, writeFile, rm } from "node:fs/promises";
import { createHash } from "node:crypto";
import { prepareComponent } from "../dist/tooling/index.js";
import { createHost, createInterpreter, directoryFilesystem } from "../dist/node.js";

const enabled = Boolean(
  process.env.TEMBRA_WASM_ENGINE && process.env.TEMBRA_WASM_ORB && process.env.TEMBRA_WASM_PROVIDER,
);

test(
  "the published Node facade executes TypeScript and Rust Wasm Orbs, reloads and physically disposes them",
  { skip: !enabled, timeout: 120000 },
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-node-orb-"),
    );
    await writeFile(path.join(directory, "input.txt"), "initial");
    const logs = [];
    const engine = createInterpreter({
      onLog(record) {
        logs.push(record);
        throw new Error("Observer failure must not interrupt guest work");
      },
      providers: { network: ["outbound"], filesystem: { data: directoryFilesystem(directory) } },
      engine: new Uint8Array(await readFile(process.env.TEMBRA_WASM_ENGINE)),
    });
    const source = JSON.parse(await readFile(process.env.TEMBRA_WASM_ORB, "utf8"));
    source.source += `
      console.log("module log");
      const originalFactory = globalThis.__tembraOrb.services.calculation.create;
      globalThis.__tembraOrb.services.calculation.create = self => {
        console.info("factory log"); return originalFactory(self);
      };
      globalThis.__tembraOrb.services.calculation.methods.recover = () => {
        console.warn("method log"); return 42;
      };`;
    source.sha256 = createHash("sha256").update(source.source).digest("hex");
    const options = {
      name: "calculator",
      apiVersion: 1,
      source,
      bindings: {
        "example:integrated/arithmetic@1.0.0": {
          kind: "orb",
          name: "arithmetic",
          api_version: 1,
          world: "arithmetic-provider",
        },
      },
    };
    const definition = engine.orb(options);
    const host = createHost();
    const provider = engine.orb({
      name: "arithmetic",
      apiVersion: 1,
      source: await prepareComponent({
        component: process.env.TEMBRA_WASM_PROVIDER,
        wit: new URL("../../../../examples/wasm-orb/contract.wit", import.meta.url).pathname,
        world: "arithmetic-provider",
      }),
    });
    try {
      await host.install([provider, definition]);
      assert.equal(engine.instances(), 2);
      const reference = await host.get("calculator::example:integrated/calculation@1.0.0", "main");
      assert.equal(await reference.sum({ values: [2, 4, 6] }), 24);
      assert.equal(await reference.read(), 24);
      assert.equal(reference.state.value, 24);
      const other = createGenericHost({ kernel: new NodeKernel() });
      try {
        await other.install([provider, definition]);
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
      assert.equal(engine.instances(), 2);
      assert.equal(await reference["resource-value"](), 42);
      await assert.rejects(reference.fail(), /Expected guest failure/);
      await assert.rejects(reference.forbidden());
      await assert.rejects(reference.sum({ values: [0.5] }));
      assert.equal(await reference.recover(), 42);
      await new Promise(setImmediate);
      for (const message of ["module log", "factory log", "method log"])
        assert(logs.some((record) => record.orb === "calculator" && record.message === message));
      assert.deepEqual(await Promise.all([reference.read(), reference.recover()]), [24, 42]);
      assert.deepEqual(
        await Promise.all(Array.from({ length: 4 }, () => reference.pending())),
        [1, 1, 1, 1],
      );
      const changed = source.source.replace(
        "Expected guest failure",
        "Expected revised guest failure",
      );
      assert.notEqual(changed, source.source);
      const replacement = engine.orb({
        ...options,
        source: {
          ...source,
          source: changed,
          sha256: createHash("sha256").update(changed).digest("hex"),
        },
      });
      const candidate = await host.prepare([replacement]);
      assert.equal(engine.instances(), 3);
      await candidate.abort();
      assert.equal(engine.instances(), 2);
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
      const server = createServer((_request, response) => {
        response.writeHead(200, { "Content-Type": "application/json", "Content-Length": "18" });
        response.end('{"values":[1,2,3]}');
      });
      server.listen(0, "127.0.0.1");
      await once(server, "listening");
      try {
        const { port } = server.address();
        const sourceOnline = { ...source, world: "online-calculator" };
        const online = engine.orb({
          ...options,
          name: "online",
          source: sourceOnline,
          bindings: {
            ...options.bindings,
            "wasi:sockets/types@0.3.0": { kind: "host", provider: "outbound" },
          },
          grants: { network: [{ provider: "outbound", connect: [`127.0.0.1:${port}`] }] },
        });
        await host.install(online);
        const network = await host.get("online::example:integrated/calculation@1.0.0", "main");
        assert.equal(await network["fetch-and-sum"](`http://127.0.0.1:${port}/values`), 12);
        network.release();
      } finally {
        await new Promise((resolve, reject) =>
          server.close((error) => (error ? reject(error) : resolve())),
        );
      }
      const filesSource =
        "globalThis.__tembraOrb={services:{files:{methods:{read:async(_e,_c,path)=>tembra.filesystem.readText(path),write:async(_e,_c,path,value)=>tembra.filesystem.writeText(path,value)}}}};";
      const files = {
        format: "tembra-interpreter-source-v1",
        source: filesSource,
        sha256: createHash("sha256").update(filesSource).digest("hex"),
        typesSha256: "",
        contract: Array.from(
          compileContract(
            new URL("../../../../crates/executor-wasm/tests/wit/storage", import.meta.url).pathname,
            "storage",
          ),
        ),
        world: "storage",
      };
      await host.install(
        engine.orb({
          name: "files",
          apiVersion: 1,
          source: files,
          bindings: {
            "wasi:filesystem/types@0.3.0": { kind: "host", provider: "data" },
            "wasi:filesystem/preopens@0.3.0": { kind: "host", provider: "data" },
            "wasi:clocks/system-clock@0.3.0": { kind: "host", provider: "system-clock" },
          },
          grants: { filesystem: [{ provider: "data", mount: "/data", writable: true }] },
        }),
      );
      const storage = await host.get("files::example:storage/files@1.0.0", "main");
      assert.equal(await storage.read("/data/input.txt"), "initial");
      await storage.write("/data/output.txt", "persisted");
      assert.equal(await readFile(path.join(directory, "output.txt"), "utf8"), "persisted");
      await assert.rejects(storage.read("/data/../outside.txt"));
      storage.release();
      reference.release();
    } finally {
      try {
        await host.close();
      } finally {
        await rm(directory, { recursive: true, force: true });
      }
    }
    assert.equal(engine.instances(), 0);
    assert.equal(host.diagnostics().values, 0);
    assert.equal(host.diagnostics().pendingJobs, 0);
  },
);
