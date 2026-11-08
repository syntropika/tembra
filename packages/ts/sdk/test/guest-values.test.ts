import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createContext, runInContext } from "node:vm";
import { compileContract, NodeContract } from "@syntropika/tembra-engine-node";

test("guest callbacks follow official top-level result exceptions while nested results stay tagged", async () => {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-values-"),
  );
  try {
    await writeFile(
      path.join(directory, "contract.wit"),
      `package example:results;
      interface values {
        type outcome = result<u32, string>;
        record wrapped { value: outcome }
        checked: func() -> outcome;
        nested: func() -> wrapped;
        plain: func() -> u32;
      }
      world test { import values; export values; }`,
    );
    const contract = new NodeContract(compileContract(directory));
    const graph = JSON.parse(contract.request('{"kind":"describe"}')).ok;
    const context = createContext({ __tembraContract: graph });
    const source = await readFile(
      new URL("../../../../crates/executor-wasm/guest/src/values.ts", import.meta.url),
      "utf8",
    );
    runInContext(new Bun.Transpiler({ loader: "ts" }).transformSync(source), context);
    const values = context.__tembraValues as {
      invoke(service: string, method: string, callback: () => unknown): Promise<unknown>;
      importResult(service: string, method: string, value: unknown): unknown;
    };
    const result = await values.invoke("values", "checked", async () => 42);
    expect(JSON.stringify(result)).toBe('{"tag":"ok","val":42}');
    const rejected = await values.invoke("values", "checked", () => {
      throw Object.assign(new Error("Rejected"), { payload: "denied" });
    });
    expect(JSON.stringify(rejected)).toBe('{"tag":"err","val":"denied"}');
    expect(values.importResult("example:results/values", "checked", result)).toBe(42);
    try {
      values.importResult("example:results/values", "checked", rejected);
      throw new Error("Expected the declared result error");
    } catch (error) {
      expect(error).toBe("denied");
    }
    const nested = { value: { tag: "err", val: "nested" } };
    expect(JSON.stringify(await values.invoke("values", "nested", () => nested))).toBe(
      JSON.stringify(nested),
    );
    expect(JSON.stringify(values.importResult("example:results/values", "nested", nested))).toBe(
      JSON.stringify(nested),
    );
    const trap = new Error("Unexpected failure");
    await expect(
      values.invoke("values", "plain", () => {
        throw trap;
      }),
    ).rejects.toBe(trap);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
