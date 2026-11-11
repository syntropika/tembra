import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { bindCaptured, type CapturedInvocation } from "../src/runtime/adapters.ts";
import { issueByteStream, receiveByteStream } from "../src/interpreter/component/streams.ts";
import { encode } from "../src/interpreter/codec.ts";

for (const installed of [false, true])
  test(`a late producer failure is reported once after ${installed ? "receiver installation" : "head abandonment"}`, async () => {
    const notifications: Error[] = [];
    const host = createHost({ onError: (error) => notifications.push(error) });
    const physical = Promise.withResolvers<void>();
    const cleanups: Promise<unknown>[] = [];
    let drops = 0;
    const method = bindCaptured(
      () => null,
      "byte-stream",
      async (_environment, context, _input, _receiver, retain) => {
        retain!(physical.promise);
        return {
          value: encode(0),
          capability: issueByteStream(
            {
              async read() {
                return { done: true };
              },
              [Symbol.dispose]() {
                drops++;
              },
            },
            physical.promise,
            context.signal,
          ),
          finished: physical.promise,
        };
      },
    );
    await host.install({
      name: "late-stream",
      apiVersion: 1,
      services: { stream: { methods: { open: method } } },
    });
    const reference = await host.get("stream", "main");
    const record = host.session.handles.reference(reference);
    try {
      await host.root(async (context) => {
        const capture: CapturedInvocation = { signature: "byte-stream", input: encode([]) };
        await host.session.calls.invokePhysical(record, "open", [], context, capture);
        expect(host.diagnostics().calls).toBe(1);
        if (!installed) return; // Nobody installs a receiver before the root drops the head.
        const owner = capture.capability;
        if (owner?.kind !== "bytes") throw new Error("Missing byte stream capability");
        const iterator = receiveByteStream(
          owner,
          context.signal,
          (operation) => {
            cleanups.push(Promise.resolve().then(operation));
          },
          (error) => host.session.report(error),
        );
        physical.reject(new Error("late producer trap"));
        await Promise.allSettled([...host.session.jobs]);
        expect(await iterator.next()).toEqual({ done: true, value: undefined });
        await Promise.allSettled(cleanups);
      });
      if (!installed) physical.reject(new Error("late producer trap"));
      await Promise.allSettled([...host.session.jobs]);
      expect(drops).toBe(1);
      expect(notifications.map((error) => error.message)).toEqual(["late producer trap"]);
      expect(host.errors().map((error) => error.message)).toEqual(["late producer trap"]);
      expect(host.diagnostics().calls).toBe(0);
    } finally {
      physical.resolve();
      reference.release();
      await host.close();
    }
  });

test("a bounded stream timeout reports promptly while shutdown retains actual provider work", async () => {
  const notices: Error[] = [];
  const reported = Promise.withResolvers<void>();
  const host = createHost({
    onError(error) {
      notices.push(error);
      reported.resolve();
    },
  });
  const bounded = Promise.withResolvers<void>();
  const actual = Promise.withResolvers<void>();
  let dropped = 0;
  const method = bindCaptured(
    () => null,
    "stream-timeout",
    async (_environment, context, _input, _receiver, retain) => {
      retain!(actual.promise);
      return {
        value: encode(0),
        capability: issueByteStream(
          {
            async read() {
              return { done: true };
            },
            [Symbol.dispose]() {
              dropped++;
            },
          },
          bounded.promise,
          context.signal,
        ),
        finished: bounded.promise,
      };
    },
  );
  await host.install({
    name: "stream-timeout",
    apiVersion: 1,
    services: { stream: { methods: { open: method } } },
  });
  const reference = await host.get("stream", "main");
  try {
    await host.root(async (context) => {
      await host.session.calls.invokePhysical(
        host.session.handles.reference(reference),
        "open",
        [],
        context,
        {
          signature: "stream-timeout",
          input: encode([]),
        },
      );
      bounded.reject(new Error("bounded stream timeout"));
      await reported.promise;
      expect(host.diagnostics().calls).toBe(1);
      expect(dropped).toBe(1);
    });
    expect(notices.map((error) => error.message)).toEqual(["bounded stream timeout"]);
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
    bounded.resolve();
    actual.resolve();
    reference.release();
    await host.close();
  }
});
