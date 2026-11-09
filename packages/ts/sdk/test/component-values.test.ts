import { expect, test } from "bun:test";
import type { NodeComponentPort } from "@syntropika/tembra-engine-node";
import { createHost } from "../src/node.ts";
import { componentDefinition } from "../src/interpreter/component.ts";

test("compiled Orbs capture portable values before crossing the native boundary", async () => {
  const received: unknown[] = [];
  let disposed = 0;
  const port: NodeComponentPort = {
    description: () =>
      JSON.stringify({
        orb: { services: [{ name: "provider::example:values/test", methods: ["read"] }] },
      }),
    async call(binding, method, args) {
      received.push({ binding, method, args: JSON.parse(args) });
      return '{"ok":7}';
    },
    close() {
      disposed++;
    },
  };
  const host = createHost();
  await host.install(componentDefinition(port, "provider", 1));
  const reference = await host.get("provider::example:values/test", "main");
  try {
    expect(await reference.read(new Int32Array([1, 2]), undefined)).toBe(7);
    expect(received).toEqual([
      { binding: "example:values/test", method: "read", args: [[1, 2], null] },
    ]);
    let coerced = false;
    for (const value of [
      -0,
      NaN,
      new Date(0),
      {
        toJSON() {
          coerced = true;
          return 1;
        },
      },
    ])
      await expect(reference.read(value)).rejects.toThrow();
    expect(coerced).toBe(false);
    expect(received).toHaveLength(1);
  } finally {
    reference.release();
    await host.close();
  }
  expect(disposed).toBe(1);
});

test("compiled service metadata must belong to its selected Orb", () => {
  const port: NodeComponentPort = {
    description: () =>
      '{"orb":{"services":[{"name":"other::example:values/test","methods":["read"]}]}}',
    call: async () => '{"ok":7}',
    close() {},
  };
  expect(() => componentDefinition(port, "provider", 1)).toThrow(
    "Invalid prepared component service",
  );
});
