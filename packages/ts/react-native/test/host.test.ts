import { afterEach, expect, mock, test } from "bun:test";
import { NodeKernel } from "@syntropika/tembra-engine-node";
import type { Spec } from "../src/NativeTembra.ts";

const kernels = new Map<string, NodeKernel>();
let sequence = 0;
let released = 0;
let failRequest = false;
const native: Spec = {
  createKernel() {
    const id = String(++sequence);
    kernels.set(id, new NodeKernel());
    return id;
  },
  request(id, metadata) {
    if (failRequest) throw new Error("Transport unavailable");
    const kernel = kernels.get(id);
    if (!kernel) throw new Error("Unknown kernel");
    return kernel.request(metadata);
  },
  releaseKernel(id) {
    released++;
    return kernels.delete(id);
  },
};
mock.module("react-native", () => ({
  TurboModuleRegistry: { get: () => native },
}));
const { createHost, orb } = await import("../src/index.ts");
const { NativeKernelPort } = await import("../src/kernel.ts");

afterEach(() => {
  kernels.clear();
  failRequest = false;
});

test("runs orb state and reload against the Rust engine, then releases its native handle", async () => {
  const define = (increment: number) =>
    orb({
      name: "counter",
      apiVersion: 1,
      services: {
        counter: {
          create(self) {
            return { state: self.state({ value: 0 }) };
          },
          methods: {
            add({ state }, context) {
              return state.update(context, (current: { value: number }) => ({
                value: current.value + increment,
              })).value;
            },
          },
        },
      },
    });
  const before = released;
  const first = createHost();
  const second = createHost();
  try {
    await first.install(define(1));
    const ref = await first.get("counter", "main");
    expect(await ref.add()).toBe(1);
    expect(second.installedOrbs()).toEqual([]);
    await first.reload([define(2)]);
    expect(await ref.add()).toBe(3);
    ref.release();
  } finally {
    await first.close();
    await second.close();
  }
  await first.close();
  expect(released - before).toBe(2);
  expect(kernels.size).toBe(0);
  expect(first.diagnostics().objects).toBe(0);
});

test("releases native storage when host construction fails", () => {
  const before = released;
  failRequest = true;
  expect(() => createHost()).toThrow("Transport unavailable");
  expect(released - before).toBe(1);
  expect(kernels.size).toBe(0);
});

test("prevents use after release and makes repeated release harmless", () => {
  const port = new NativeKernelPort(native);
  expect(JSON.parse(port.request('{"op":"features"}')).ok).toBeDefined();
  const before = released;
  port.free();
  port.free();
  expect(released - before).toBe(1);
  expect(() => port.request("{}")).toThrow("released");
});
