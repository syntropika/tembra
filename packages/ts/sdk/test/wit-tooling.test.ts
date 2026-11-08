import { expect, test } from "bun:test";
import { access, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { build } from "../src/tooling/index.ts";
import { NodeArtifact } from "@syntropika/tembra-engine-node";
import { readFile } from "node:fs/promises";

test("WIT builds produce Wasm data and editor declarations, reject wrong values and invalidate after contract edits", async () => {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-wit-"),
  );
  const contract =
    "package example:typed; interface calculation { record input { value: u32 } read: func(input: input) -> u32; } world calculator { export calculation; }";
  const source =
    'import type {calculation} from "./contract.wit"; const value: calculation.Input = {value:42}; export default {name:"typed",apiVersion:1,services:{calculation:{methods:{read:()=>value.value}}}} as const;';
  try {
    await writeFile(path.join(directory, "contract.wit"), contract);
    await writeFile(path.join(directory, "orb.ts"), source);
    const prepare = async () =>
      (
        await build({
          root: directory,
          builds: { typed: { entry: "orb.ts", wit: "contract.wit" } },
        })
      )[0]!;
    const first = await prepare();
    const decoded = new NodeArtifact(new Uint8Array(await readFile(first.path)));
    expect(JSON.parse(decoded.description())).toMatchObject({ name: "typed", kind: "source" });
    await access(path.join(directory, "contract.d.wit.ts"));
    await access(path.join(directory, ".tembra"));
    expect(first.path).toEndWith(".wasm");
    await writeFile(path.join(directory, "orb.ts"), source.replace("value:42", 'value:"wrong"'));
    await expect(prepare()).rejects.toThrow("not assignable");
    await writeFile(
      path.join(directory, "contract.wit"),
      contract.replace("value: u32", "value: string"),
    );
    await writeFile(path.join(directory, "orb.ts"), source);
    await expect(prepare()).rejects.toThrow("not assignable");
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
