import { expect, test } from "bun:test";
import { ComponentPort } from "../src/interpreter/component/port.ts";
import type { CallbackScope } from "../src/interpreter/component/scope.ts";
import { httpImports } from "../src/interpreter/component/wasi-http.ts";
import { HttpFields, type HttpRequest } from "../src/interpreter/standard-http-values.ts";

for (const phase of ["before import", "before provider microtask"])
  test(`compiled HTTP does not start provider work cancelled ${phase}`, async () => {
    const controller = new AbortController();
    const port = new ComponentPort(1000, async () => {});
    port.exports = {};
    let starts = 0;
    const imports = httpImports(port.wasi, {
      provider: () => ({
        async send() {
          starts++;
          return { status: 200, fields: [], body: null };
        },
      }),
    });
    const Request = imports.types.Request as {
      new: (
        headers: HttpFields,
        body: undefined,
        trailers: Promise<unknown>,
        options: undefined,
      ) => [HttpRequest, unknown];
    };
    const [request] = Request.new(
      new HttpFields(),
      undefined,
      Promise.resolve({ tag: "ok" }),
      undefined,
    );
    request.setScheme({ tag: "HTTPS" });
    request.setAuthority("example.test");
    request.setPathWithQuery("/");
    const scope = {
      signal: controller.signal,
      check() {},
      cancel() {},
    } as unknown as CallbackScope;
    const callback = port.start(1n, scope, async () => {
      port.observer.taskStarted({ id: 1, entry: "http", isAsync: true });
      if (phase === "before import") controller.abort();
      else queueMicrotask(() => controller.abort());
      try {
        return await (imports.client.send as (request: HttpRequest) => Promise<unknown>)(request);
      } finally {
        port.observer.taskExited({ id: 1 });
      }
    });
    try {
      expect(await callback.result).toEqual({
        tag: "err",
        val: { tag: "internal-error", val: "request was cancelled" },
      });
      await callback.physical;
      expect(starts).toBe(0);
    } finally {
      await port.close();
    }
  });
