import { expect, test } from "bun:test";
import { withIoCancellation } from "../src/interpreter/types.ts";
import type { IORequest } from "../src/interpreter/host-protocol.ts";

const connect: IORequest = { operation: "transport.connect", ip: "127.0.0.1", port: 8080 };

/** A pending operation that settles only when told to. */
function pending() {
  let settle!: (value: string) => void;
  let fail!: (error: Error) => void;
  const operation = new Promise<string>((resolve, reject) => {
    settle = resolve;
    fail = reject;
  });
  return { operation, settle, fail };
}

test("native and mobile ports revoke source I/O only while an operation is pending", async () => {
  const controller = new AbortController();
  let cancels = 0;
  const { operation, fail } = pending();
  const forwarded = withIoCancellation(operation, connect, controller.signal, () => {
    cancels++;
    fail(new Error("callback ended before the connection opened"));
  });
  controller.abort();
  await expect(forwarded).rejects.toThrow("callback ended");
  expect(cancels).toBe(1);

  // A later abort of the same signal cannot reach another callback's operation.
  const later = new AbortController();
  const second = pending();
  const result = withIoCancellation(second.operation, connect, later.signal, () => cancels++);
  second.settle("{}");
  expect(await result).toBe("{}");
  later.abort();
  expect(cancels).toBe(1);
});

test("cleanup survives cancellation and an already aborted signal revokes at once", async () => {
  const aborted = AbortSignal.abort();
  let cancels = 0;
  for (const request of [{ operation: "transport.close", id: 1 }] as const)
    expect(
      await withIoCancellation(Promise.resolve("null"), request, aborted, () => cancels++),
    ).toBe("null");
  expect(cancels).toBe(0);
  expect(await withIoCancellation(Promise.resolve("1"), connect, aborted, () => cancels++)).toBe(
    "1",
  );
  expect(cancels).toBe(1);
  expect(await withIoCancellation(Promise.resolve("1"), connect, undefined, () => cancels++)).toBe(
    "1",
  );
  expect(cancels).toBe(1);
});
