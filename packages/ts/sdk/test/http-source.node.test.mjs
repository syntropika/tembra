import { createServer } from "node:http";
import { once } from "node:events";
import { readFile } from "node:fs/promises";
import path from "node:path";
import assert from "node:assert/strict";
import { test } from "node:test";
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
    const server = createServer((request, response) => {
      if (request.url === "/upload") {
        const state = { bytes: 0, invalid: false, completed: false, closed: false };
        uploads.push(state);
        request.on("data", (bytes) => {
          for (let index = 0; index < bytes.length; index++)
            if (bytes[index] !== Math.floor((state.bytes + index) / 65536) % 251)
              state.invalid = true;
          state.bytes += bytes.length;
        });
        request.on("end", () => {
          state.completed = true;
          response.end(String(state.bytes));
        });
        request.on("error", () => {});
        request.on("close", () => {
          state.closed = true;
        });
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
    const engine = process.env.TEMBRA_WASM_ENGINE;
    const host = createHost({
      isolated: {
        ...(engine ? { engine: new Uint8Array(await readFile(engine)) } : {}),
        providers: { network: ["outbound"] },
        // The 3 MiB scenario measures streaming, not the default small callback fuel budget.
        limits: { fuel: 500_000_000 },
      },
    });
    let requests;
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
      stage = "throwing upload";
      await assert.rejects(requests.upload(`${origin}/upload`, 4, true));
      assert.equal(
        uploads[1].completed,
        false,
        "a throwing producer must not finish an HTTP upload",
      );
      assert.equal(uploads[1].closed, true, "callback completion waits for the aborted peer");
      assert.ok(uploads[1].bytes < 4 * 65536);
      stage = "retained response";
      assert.equal(await requests.open(`${origin}/retained`), 200);
      assert.equal(await requests.read(), "hé🌍");
      stage = "dropped response";
      assert.equal(await requests.open(`${origin}/drop`), 200);
      await requests.close();
      await assert.rejects(requests.read(), /No response/);
    } catch (error) {
      throw new Error(`${stage}: ${error}`, { cause: error });
    } finally {
      requests?.release();
      try {
        await host.close();
      } finally {
        for (const socket of sockets) socket.destroy();
        const closed = once(server, "close");
        server.close();
        await closed;
      }
    }
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
  },
);
