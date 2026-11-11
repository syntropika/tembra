import { createServer } from "node:http";
import { once } from "node:events";
import { readFile } from "node:fs/promises";
import path from "node:path";
import assert from "node:assert/strict";
import { test } from "node:test";
import { cleanup, closeQuarantined } from "./cleanup.ts";
import { createHost } from "../dist/node.js";

const fixtures = process.env.TEMBRA_WASM_FIXTURES;

test(
  "TypeScript HTTP streams uploads and trailers, refuses truncation, and retains responses",
  {
    skip: !fixtures,
    timeout: 60000,
  },
  async () => {
    const sockets = new Set();
    const uploads = [];
    let acknowledged = false;
    let observed;
    const interruptedStarted = new Promise((resolve) => {
      observed = resolve;
    });
    const server = createServer((request, response) => {
      if (request.url === "/upload-started") {
        const timer = setTimeout(() => response.writeHead(504).end(), 2000);
        response.once("close", () => clearTimeout(timer));
        void interruptedStarted.then(() => {
          clearTimeout(timer);
          if (!response.destroyed) {
            response.once("finish", () => {
              acknowledged = true;
            });
            response.end("started");
          }
        });
        return;
      }
      if (request.url === "/upload" || request.url === "/interrupted-upload") {
        const closed = new Promise((resolve) => request.once("close", resolve));
        const state = { bytes: 0, invalid: false, completed: false, closed };
        uploads.push(state);
        request.on("data", (bytes) => {
          for (let index = 0; index < bytes.length; index++)
            if (bytes[index] !== Math.floor((state.bytes + index) / 65536) % 251)
              state.invalid = true;
          state.bytes += bytes.length;
          if (request.url === "/interrupted-upload" && state.bytes > 0) observed();
        });
        request.on("end", () => {
          state.completed = true;
          response.end(String(state.bytes));
        });
        request.on("error", () => {});
        return;
      }
      response.writeHead(200, {
        "Content-Type": "text/plain; charset=utf-8",
        "X-Repeat": ["one", "two"],
        Trailer: "X-Finished",
      });
      response.write("hé🌍");
      response.addTrailers({ "X-Finished": "complete" });
      response.end();
    });
    server.on("connection", (socket) => {
      sockets.add(socket);
      socket.on("close", () => sockets.delete(socket));
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const origin = `http://127.0.0.1:${server.address().port}`;
    const host = createHost({
      isolated: {
        providers: { network: ["outbound"] },
        // The 3 MiB scenario measures streaming, not the default small callback fuel budget.
        limits: { fuel: 500_000_000 },
      },
    });
    let requests;
    let failure;
    let stage = "install";
    try {
      await host.install({
        artifact: new Uint8Array(await readFile(path.join(fixtures, "http.wasm"))),
        grants: { http: [{ provider: "outbound", origins: [origin] }] },
      });
      requests = await host.get("http::example:http/requests@1.0.0", "main");
      stage = "response fields and trailers";
      assert.deepEqual(await requests.inspect(`${origin}/inspect`), {
        status: 200,
        text: "hé🌍",
        duplicateCount: 2,
        trailer: "complete",
      });
      stage = "streamed upload";
      assert.equal(await requests.upload(`${origin}/upload`, 48, false), String(3 * (1 << 20)));
      assert.equal(uploads[0].bytes, 3 * (1 << 20));
      assert.equal(uploads[0].invalid, false);
      assert.equal(uploads[0].completed, true);
      stage = "retained response";
      assert.equal(await requests.open(`${origin}/retained`), 200);
      assert.equal(await requests.read(), "hé🌍");
      stage = "dropped response";
      assert.equal(await requests.open(`${origin}/drop`), 200);
      await requests.close();
      // A declared result lane keeps the refusal recoverable.
      assert.deepEqual(await requests["read-checked"](), {
        tag: "err",
        val: "No response is open",
      });
      assert.equal(await requests.open(`${origin}/retained`), 200);
      assert.deepEqual(await requests["read-checked"](), { tag: "ok", val: "hé🌍" });
    } catch (error) {
      failure = new Error(`${stage}: ${error}`, { cause: error });
    }
    requests?.release();
    // Cleanup reports its own failure together with the stage failure; neither is masked.
    try {
      await cleanup(failure, () => host.close());
      if (failure) throw failure;
      const diagnostics = host.diagnostics();
      for (const name of [
        "objects",
        "references",
        "roots",
        "calls",
        "resources",
        "tasks",
        "orbs",
        "values",
        "pendingJobs",
      ])
        assert.equal(diagnostics[name], 0, `${name} after retirement`);
      // Strict case on its own host: a throwing upload producer traps before its writer is
      // dropped and quarantines the revision. Its retained `response` resource cannot be closed
      // by a quarantined revision, so closing that host reports the cleanup failure honestly.
      const isolated = createHost({ isolated: { providers: { network: ["outbound"] } } });
      let closed = false;
      try {
        await isolated.install({
          artifact: new Uint8Array(await readFile(path.join(fixtures, "http.wasm"))),
          grants: { http: [{ provider: "outbound", origins: [origin] }] },
        });
        const strict = await isolated.get("http::example:http/requests@1.0.0", "main");
        await assert.rejects(
          strict.interrupt(`${origin}/interrupted-upload`, `${origin}/upload-started`),
        );
        assert.equal(acknowledged, true, "the peer must acknowledge before the producer fails");
        assert.ok(uploads[1].bytes > 0, "the peer must observe bytes before the producer fails");
        assert.equal(
          uploads[1].completed,
          false,
          "a throwing producer must not finish an HTTP upload",
        );
        // The transport awaits its local close; the peer observes it on a later event-loop turn.
        let timer;
        try {
          await Promise.race([
            uploads[1].closed,
            new Promise((_, reject) => {
              timer = setTimeout(
                () => reject(new Error("the aborted upload kept its peer open")),
                2000,
              );
            }),
          ]);
        } finally {
          clearTimeout(timer);
        }
        assert.ok(uploads[1].bytes < 4 * 65536);
        await assert.rejects(strict.inspect(`${origin}/inspect`), /quarantined/);
        strict.release();
        await closeQuarantined(() => isolated.close());
        closed = true;
      } finally {
        if (!closed) await isolated.close().catch(() => {});
      }
    } finally {
      for (const socket of sockets) socket.destroy();
      const serverClosed = once(server, "close");
      server.close();
      await serverClosed;
    }
  },
);
