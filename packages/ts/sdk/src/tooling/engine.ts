import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import { transpileBytes } from "@bytecodealliance/jco-transpile";

/** Prepare the reusable trusted engine once for browser installation; Orb edits reuse these files. */
export async function prepareBrowserEngine(
  component: Uint8Array,
  outDir: string,
): Promise<{ entry: string; sha256: string; bytes: number }> {
  const result = await transpileBytes(component, {
    name: "interpreter",
    instantiation: "async",
    asyncMode: "jspi",
    asyncExports: ["load", "dispatch"],
    nodejsCompat: false,
    base64Cutoff: 0,
  });
  const root = path.resolve(outDir);
  await mkdir(root, { recursive: true });
  for (const [name, data] of Object.entries(result.files)) {
    const file = path.resolve(root, name);
    if (!file.startsWith(`${root}${path.sep}`))
      throw new Error("Engine output escaped its destination");
    await mkdir(path.dirname(file), { recursive: true });
    await writeFile(file, data);
  }
  return {
    entry: path.join(root, "interpreter.js"),
    sha256: createHash("sha256").update(component).digest("hex"),
    bytes: component.byteLength,
  };
}
