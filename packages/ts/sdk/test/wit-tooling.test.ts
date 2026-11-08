import { expect, test } from "bun:test";
import { access, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { prepareSource, witTypes } from "../src/tooling/index.ts";

test("direct WIT types remain virtual, reject wrong values and invalidate after contract edits", async () => {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-wit-"),
  );
  const contract =
    "package example:typed; interface calculation { record input { value: u32 } read: func(input: input) -> u32; } world calculator { export calculation; }";
  const source =
    'import type {calculation} from "./contract.wit"; const value: calculation.Input = {value:42}; export default {services:{calculation:{methods:{read:()=>value.value}}}};';
  try {
    await writeFile(path.join(directory, "contract.wit"), contract);
    await writeFile(path.join(directory, "orb.ts"), source);
    const first = await prepareSource({ root: directory, entry: "orb.ts", wit: "contract.wit" });
    expect(first.source).toContain("42");
    expect(first.source).not.toContain("contract.wit");
    await expect(access(path.join(directory, ".tembra"))).rejects.toThrow();
    await writeFile(path.join(directory, "orb.ts"), source.replace("value:42", 'value:"wrong"'));
    await expect(
      prepareSource({ root: directory, entry: "orb.ts", wit: "contract.wit" }),
    ).rejects.toThrow("not assignable");
    await writeFile(
      path.join(directory, "contract.wit"),
      contract.replace("value: u32", "value: string"),
    );
    const changed = await witTypes(path.join(directory, "contract.wit"));
    expect(changed.sha256).not.toBe(first.typesSha256);
    await writeFile(path.join(directory, "orb.ts"), source);
    await expect(
      prepareSource({ root: directory, entry: "orb.ts", wit: "contract.wit" }),
    ).rejects.toThrow("not assignable");
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
