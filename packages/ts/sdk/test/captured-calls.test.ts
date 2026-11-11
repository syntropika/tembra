import { expect, test } from "bun:test";
import { createHost, orb } from "../src/node.ts";
import { bindCaptured, type CapturedInvocation } from "../src/runtime/adapters.ts";
import { encode } from "../src/interpreter/codec.ts";
import { deferred } from "../src/runtime/records.ts";

test("captured calls follow the admitted handler and exact signature across reload", async () => {
  let ordinary = 0;
  let captured = 0;
  const prepared = bindCaptured(
    (_environment, _context, value: number) => {
      ordinary++;
      return value + 1;
    },
    "contract-one",
    async (_environment, _context, input) => {
      captured++;
      return encode(JSON.parse(input.text)[0] + 1);
    },
  );
  const definition = (handle: typeof prepared) =>
    orb({
      name: "provider",
      apiVersion: 1,
      services: { provider: { methods: { calculate: handle } } },
    });
  const host = createHost();
  await host.install(definition(prepared));
  const reference = await host.get("provider", "main");
  const record = host.session.handles.reference(reference);
  const invoke = async (signature: string, value: number) => {
    const capture: CapturedInvocation = { signature, input: encode([value]) };
    const result = await host.root((ctx) =>
      host.session.calls.invoke(record, "calculate", [value], ctx, capture),
    );
    return { result, output: capture.output?.text };
  };
  try {
    expect(await invoke("contract-one", 7)).toEqual({ result: undefined, output: "8" });
    expect(captured).toBe(1);
    expect(ordinary).toBe(0);
    expect(await invoke("contract-two", 8)).toEqual({ result: 9, output: undefined });
    expect(ordinary).toBe(1);
    // A replacement ordinary implementation receives no channel, even with the same service name.
    await host.reload([definition((_environment, _context, value: number) => ({ value }))]);
    expect(await invoke("contract-one", 9)).toEqual({ result: { value: 9 }, output: undefined });
    expect(captured).toBe(1);
  } finally {
    reference.release();
    await host.close();
  }
});

test("cancellation stops a call waiting on readiness before any handler is admitted", async () => {
  const host = createHost();
  let invoked = 0;
  await host.install(
    orb({
      name: "pending",
      apiVersion: 1,
      services: {
        pending: {
          methods: {
            run: () => {
              invoked++;
            },
          },
        },
      },
    }),
  );
  const reference = await host.get("pending", "main");
  const record = host.session.handles.reference(reference);
  const pending = deferred<void>();
  record.ready = pending.promise;
  try {
    await host.root(async (context) => {
      const running = host.session.calls.invoke(record, "run", [], context);
      context.cancel();
      await expect(running).rejects.toMatchObject({ code: "cancelled", reason: "root" });
      expect(invoked).toBe(0);
    });
    pending.resolve();
    await Promise.resolve();
    expect(invoked).toBe(0);
  } finally {
    pending.resolve();
    reference.release();
    await host.close();
  }
});

test("ordinary prepared data callers retain actual work after their bounded result fails", async () => {
  const host = createHost();
  const actual = Promise.withResolvers<void>();
  let ordinaryFallback = 0;
  let checkedData = 0;
  const method = bindCaptured(
    () => {
      ordinaryFallback++;
      return 0;
    },
    "data-retention",
    async () => {
      throw new Error("capture-only path");
    },
    async (_environment, _context, args, retain) => {
      checkedData++;
      expect(args).toEqual([7]);
      retain(actual.promise);
      throw new Error("bounded prepared data timeout");
    },
  );
  await host.install({
    name: "data-retention",
    apiVersion: 1,
    services: { data: { methods: { read: method } } },
  });
  const reference = await host.get("data", "main");
  try {
    await expect(reference.read(7)).rejects.toThrow("bounded prepared data timeout");
    expect(checkedData).toBe(1);
    expect(ordinaryFallback).toBe(0);
    expect(host.diagnostics().calls).toBe(1);
    reference.release();
    let closed = false;
    const closing = host.close().then(() => {
      closed = true;
    });
    await Promise.resolve();
    expect(closed).toBe(false);
    actual.resolve();
    await closing;
    expect(host.diagnostics().calls).toBe(0);
  } finally {
    actual.resolve();
    reference.release();
    await host.close();
  }
});
