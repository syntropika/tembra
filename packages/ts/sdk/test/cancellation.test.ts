import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import type { Context } from "../src/runtime/context.ts";
import { KernelOp } from "../src/protocol.ts";
import { deferred } from "../src/runtime/records.ts";

test("an admitted foreign dependency settles physically after its caller cancels", async () => {
  const host = createHost();
  const entered = deferred<void>();
  const resume = deferred<void>();
  let settled = false;
  try {
    await host.install({
      name: "provider",
      apiVersion: 1,
      services: {
        "provider.work": {
          methods: {
            async run() {
              entered.resolve();
              await resume.promise;
              settled = true;
              return 42;
            },
          },
        },
      },
    });
    const reference = await host.get("provider.work", "main");
    const record = host.session.handles.reference(reference);
    await host.root(async (context) => {
      const pending = host.session.calls.invokePhysical(record, "run", [], context);
      await entered.promise;
      context.cancel();
      expect(settled).toBe(false);
      await expect(
        host.session.calls.invokePhysical(record, "run", [], context),
      ).rejects.toMatchObject({ code: "cancelled" });
      resume.resolve();
      expect(await pending).toBe(42);
      expect(settled).toBe(true);
    });
    reference.release();
  } finally {
    resume.resolve();
    await host.close();
  }
});

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
    if (close) host.session.lifecycle.flush();
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

test("a fresh root needs no extra signal check and refuses access after expiry", async () => {
  const host = createHost();
  const commands = spyOn(host.session.kernel.port, "command");
  const checks = () => commands.mock.calls.filter(([op]) => op === KernelOp.CheckRoot).length;
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
    expect(checks()).toBe(0);
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

test("foreign issued roots still check cancellation before creating their signal mirror", async () => {
  const host = createHost();
  const root = host.session.kernel.command(KernelOp.Root, { principal: "foreign" });
  host.session.kernel.command(KernelOp.CancelRoot, { root });
  try {
    await host.session.calls.root(
      (ctx) => {
        expect(ctx.signal.aborted).toBe(true);
        expect(ctx.signal.reason).toMatchObject({ code: "cancelled", reason: "root" });
      },
      false,
      root,
    );
    expect(host.session.controllers.size).toBe(0);
  } finally {
    await host.close();
  }
});
