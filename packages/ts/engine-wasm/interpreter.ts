import { mkdir, writeFile, rm } from "node:fs/promises";
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
    asyncExports: ["load", "declare", "start", "resume"],
    asyncImports: [
      "wasi:io/poll#poll",
      "wasi:io/poll#[method]pollable.block",
      "wasi:io/streams#[method]input-stream.blocking-read",
      "wasi:io/streams#[method]input-stream.blocking-skip",
      "wasi:io/streams#[method]output-stream.blocking-flush",
      "wasi:io/streams#[method]output-stream.blocking-write-and-flush",
      "wasi:io/streams#[method]output-stream.blocking-write-zeroes-and-flush",
      "wasi:io/streams#[method]output-stream.blocking-splice",
      "wasi:filesystem/types#[method]descriptor.open-at",
      "wasi:filesystem/types#[method]descriptor.read",
      "wasi:filesystem/types#[method]descriptor.write",
    ],
    nodejsCompat: false,
    base64Cutoff: 0,
    minify: true,
  });
  const root = path.resolve(outDir);
  await rm(root, { recursive: true, force: true });
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
