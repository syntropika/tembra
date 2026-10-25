import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import type { Context } from "../src/runtime/context.ts";
import { KernelOp } from "../src/protocol.ts";
import { deferred } from "../src/runtime/records.ts";

for (const close of [false, true])
  test(`a signal first requested after ${close ? "host close" : "explicit cancellation"} is aborted`, async () => {
    const host = createHost();
    const started = deferred<void>();
    const resume = deferred<void>();
    const running = host.root(async (ctx) => {
      started.resolve();
      await resume.promise;
      if (!close) ctx.cancel();
      const signal = ctx.signal;
      expect(signal.aborted).toBe(true);
      expect(signal.reason).toMatchObject({ code: "cancelled", reason: "root" });
      expect(ctx.signal).toBe(signal);
    });
    await started.promise;
    const closing = close ? host.close() : undefined;
    if (close) host.session.flush();
    resume.resolve();
    try {
      await running;
      await closing;
      expect(host.session.controllers.size).toBe(0);
    } finally {
      await host.close();
    }
  });

test("a cancellation delivered as its root ends does not retain a controller", async () => {
  const host = createHost();
  await host.root((ctx) => {
    host.session.kernel.command(KernelOp.CancelRoot, { root: ctx.root });
  });
  expect(host.session.controllers.size).toBe(0);
  await host.close();
});

test("one root checks its lazy signal once and refuses first access after expiry", async () => {
  const host = createHost();
  let checks = 0;
  const request = host.session.kernel.port.request.bind(host.session.kernel.port);
  const commands = spyOn(host.session.kernel.port, "request").mockImplementation((metadata) => {
    if (JSON.parse(metadata).op === KernelOp.CheckRoot) checks++;
    return request(metadata);
  });
  let expired: Context | undefined;
  try {
    await host.root((ctx) => {
      const first = ctx.signal;
      expect(ctx.signal).toBe(first);
      expect(first.aborted).toBe(false);
      ctx.cancel();
      expect(first.aborted).toBe(true);
      expect(ctx.signal).toBe(first);
    });
    expect(checks).toBe(1);
    await host.root((ctx) => {
      expired = ctx;
    });
    expect(() => expired!.signal).toThrow(
      expect.objectContaining({ code: "expired", reason: "root" }),
    );
    expect(host.session.controllers.size).toBe(0);
  } finally {
    commands.mockRestore();
    await host.close();
  }
});
