import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import { dev, ToolingError } from "@tembra/sdk/tooling";
import { deferredVoid } from "@tembra/testing";

test.each([false, true])(
  "AbortSignal shutdown joins owned asynchronous cleanup and retains its failure (failure=%s)",
  async (closeFailure) => {
    const root = await mkdtemp(path.join(tmpdir(), "tembra-session-cleanup-"));
    const started = deferredVoid();
    const release = deferredVoid();
    const original = new Error("owned close failed", {
      cause: new Error("original cleanup cause"),
    });
    const listener = Bun.serve({
      port: 0,
      fetch() {
        return new Response("owned listener");
      },
    });
    let closes = 0;
    let otherClosed = false;
    const controller = new AbortController();
    try {
      await writeFile(path.join(root, "facet.ts"), "export default {};\n");
      const session = await dev(
        { root, builds: { main: { entry: "facet.ts", platform: "server" } } },
        {
          signal: controller.signal,
          hosts: {
            server: {
              async apply() {
                return { status: "applied" };
              },
              async close() {
                closes += 1;
                started.resolve();
                await release.promise;
                await listener.stop(true);
                if (closeFailure) {
                  throw original;
                }
              },
            },
            browser: {
              async apply() {
                return { status: "applied" };
              },
              async close() {
                otherClosed = true;
              },
            },
          },
        },
      );
      try {
        await session.ready;
        controller.abort(new Error("embedding application shutdown"));
        let settled = false;
        const closing = session.close();
        const observed = closing.then(
          () => {
            settled = true;
          },
          () => {
            settled = true;
          },
        );
        await started.promise;
        assert.equal(settled, false);
        const response = await fetch(listener.url);
        assert.equal(await response.text(), "owned listener");
        release.resolve();
        await (closeFailure ? assert.rejects(closing, (error) => error === original) : closing);
        await observed;
        assert.equal(closes, 1);
        assert.equal(otherClosed, true);
      } finally {
        release.resolve();
        await session.close().catch(() => undefined);
      }
    } finally {
      release.resolve();
      await listener.stop(true);
      await rm(root, { recursive: true, force: true });
    }
  },
);

test("initial scan and adopted host cleanup preserve both original failures", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-session-init-cleanup-"));
  const file = path.join(root, "not-a-directory");
  const original = new Error("owned cleanup failed", {
    cause: new Error("original cleanup cause"),
  });
  try {
    await writeFile(file, "authored source");
    await assert.rejects(
      dev(
        { root: file, builds: {} },
        {
          hosts: {
            server: {
              async apply() {
                return { status: "applied" };
              },
              async close() {
                throw original;
              },
            },
          },
        },
      ),
      (error) => {
        assert.ok(error instanceof ToolingError && error.phase === "watch");
        assert.ok(error.cause instanceof AggregateError);
        const scan: unknown = error.cause.errors[0];
        const cleanup: unknown = error.cause.errors[1];
        assert.ok(scan instanceof Error && "code" in scan && scan.code === "ENOTDIR");
        assert.equal(cleanup, original);
        assert.equal(error.cause.cause, scan);
        return true;
      },
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
