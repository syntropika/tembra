import { expect, test } from "bun:test";
import { createRequire } from "node:module";
import { verifyIdentifierPrecision } from "./precision.ts";
import type { KernelPort } from "../src/kernel.ts";

const { NodeKernel } = createRequire(import.meta.url)("@tembra/engine-node") as {
  NodeKernel: new () => KernelPort;
};

test("native JSON identities preserve neighboring safe integers through storage and cleanup", () => {
  expect(verifyIdentifierPrecision(() => new NodeKernel())).toBe(9);
});
