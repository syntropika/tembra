import { expect, test } from "bun:test";
import { checkSignal, combinedSignal } from "../src/runtime/signals.ts";

/** React Native's AbortSignal exposes events and aborted, without newer DOM conveniences. */
function mobileController() {
  const events = new EventTarget();
  const signal = Object.assign(events, { aborted: false }) as AbortSignal;
  return {
    signal,
    abort: () => {
      (signal as { aborted: boolean }).aborted = true;
      signal.dispatchEvent(new Event("abort"));
    },
  };
}

test("mobile cancellation works without throwIfAborted or signal reasons", () => {
  const controller = mobileController();
  checkSignal(controller.signal);
  controller.abort();
  expect(() => checkSignal(controller.signal)).toThrow();
});

test("fallback signals observe owner cancellation and already-ended callback lifetimes", () => {
  const original = AbortSignal.any;
  try {
    Object.defineProperty(AbortSignal, "any", { configurable: true, value: undefined });
    const owner = mobileController(),
      ended = mobileController();
    const combined = combinedSignal(owner.signal, ended.signal);
    expect(combined.aborted).toBe(false);
    owner.abort();
    expect(combined.aborted).toBe(true);
    ended.abort();
    expect(combinedSignal(mobileController().signal, ended.signal).aborted).toBe(true);
  } finally {
    Object.defineProperty(AbortSignal, "any", { configurable: true, value: original });
  }
});

test("installation snapshots grants without a structuredClone global", async () => {
  const { prepareDefinitions } = await import("../src/runtime/adapters.ts");
  const original = globalThis.structuredClone;
  const artifact = {
    artifact: new Uint8Array([1, 2]),
    requires: { dependency: { orb: "provider", apiVersion: 1 } },
    grants: {
      filesystem: [{ provider: "files", mount: "/data", writable: false }],
      network: [{ provider: "tcp", connect: ["127.0.0.1:80"] }],
    },
  };
  let release!: () => void;
  const pending = new Promise<void>((resolve) => {
    release = resolve;
  });
  let captured: typeof artifact | undefined;
  try {
    Object.defineProperty(globalThis, "structuredClone", { configurable: true, value: undefined });
    const preparation = prepareDefinitions([artifact], async (definition) => {
      captured = definition as typeof artifact;
      await pending;
      return { name: "mobile", apiVersion: 1 };
    });
    artifact.requires.dependency.orb = "changed";
    artifact.grants.filesystem[0]!.writable = true;
    artifact.grants.network[0]!.connect[0] = "127.0.0.1:99";
    artifact.artifact[0] = 9;
    release();
    await preparation;
    expect(captured?.requires.dependency.orb).toBe("provider");
    expect(captured?.grants.filesystem[0]?.writable).toBe(false);
    expect(captured?.grants.network[0]?.connect).toEqual(["127.0.0.1:80"]);
    expect(captured?.artifact[0]).toBe(1);
  } finally {
    release();
    Object.defineProperty(globalThis, "structuredClone", { configurable: true, value: original });
  }
});
