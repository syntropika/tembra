import { mkdir, writeFile, rm } from "node:fs/promises";
import path from "node:path";
import { createHash } from "node:crypto";
import * as abi from "../../../packages/ts/sdk/src/interpreter/abi.ts";

interface Import {
  module: string;
  name: string;
  kind: number;
  minimum?: number;
  maximum?: number;
}

/** Read one unsigned LEB128 value. */
function leb(bytes: Uint8Array, cursor: { at: number }): number {
  let value = 0,
    shift = 0,
    byte: number;
  do {
    if (cursor.at >= bytes.length || shift > 35) throw new Error("Malformed interpreter module");
    byte = bytes[cursor.at++]!;
    value += (byte & 0x7f) * 2 ** shift;
    shift += 7;
  } while (byte & 0x80);
  return value;
}

/** Parse the import section, including memory limits that the JavaScript API does not expose. */
function interpreterImports(bytes: Uint8Array): Import[] {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  if (bytes.length < 8 || view.getUint32(0, true) !== 0x6d736100 || view.getUint32(4, true) !== 1)
    throw new Error("The interpreter must be a WebAssembly core module");
  const text = new TextDecoder("utf-8", { fatal: true });
  const cursor = { at: 8 };
  const imports: Import[] = [];
  while (cursor.at < bytes.length) {
    const id = bytes[cursor.at++]!;
    const size = leb(bytes, cursor);
    const end = cursor.at + size;
    if (id === 2) {
      for (let count = leb(bytes, cursor); count > 0; count--) {
        const name = (): string => {
          const length = leb(bytes, cursor);
          const value = text.decode(bytes.subarray(cursor.at, cursor.at + length));
          cursor.at += length;
          return value;
        };
        const entry: Import = { module: name(), name: name(), kind: bytes[cursor.at++]! };
        if (entry.kind === 0) leb(bytes, cursor);
        else if (entry.kind === 2) {
          const flags = leb(bytes, cursor);
          if (flags & ~1) throw new Error("The interpreter memory must be unshared 32-bit memory");
          entry.minimum = leb(bytes, cursor);
          if (flags & 1) entry.maximum = leb(bytes, cursor);
        } else throw new Error("The interpreter may import only functions and memory");
        imports.push(entry);
      }
    }
    cursor.at = end;
  }
  return imports;
}

/** Check the exact private ABI: host-bounded memory, allowlisted functions and required exports. */
export async function checkEngine(bytes: Uint8Array): Promise<void> {
  const memory = interpreterImports(bytes).filter((entry) => entry.kind === 2);
  if (
    memory.length !== 1 ||
    memory[0]!.module !== abi.MEMORY_MODULE ||
    memory[0]!.name !== abi.MEMORY_IMPORT ||
    memory[0]!.minimum !== abi.MEMORY_INITIAL_PAGES ||
    memory[0]!.maximum !== undefined
  )
    throw new Error("The interpreter must import exactly its initial memory without a maximum");
  const module = await WebAssembly.compile(bytes);
  const functions = WebAssembly.Module.imports(module)
    .filter((entry) => entry.kind === "function")
    .map((entry) => `${entry.module}::${entry.name}`)
    .sort();
  const expected = [
    `${abi.IMPORT_MODULE}::${abi.SYNC_IMPORT}`,
    `${abi.IMPORT_MODULE}::${abi.SYNC_READ_IMPORT}`,
    ...abi.WASI_IMPORTS.map((name) => `${abi.WASI_MODULE}::${name}`),
  ].sort();
  if (JSON.stringify(functions) !== JSON.stringify(expected))
    throw new Error(`Interpreter imports differ from the ABI allowlist: ${functions.join(", ")}`);
  const exports = WebAssembly.Module.exports(module).map((entry) => `${entry.kind}:${entry.name}`);
  for (const name of [abi.INITIALIZE_EXPORT, abi.ALLOC_EXPORT, abi.CALL_EXPORT])
    if (!exports.includes(`function:${name}`))
      throw new Error(`Interpreter export ${name} is missing`);
}

/** Prepare the reusable trusted engine once for browser installation; Orb edits reuse it. */
export async function prepareBrowserEngine(
  module: Uint8Array,
  outDir: string,
): Promise<{ entry: string; sha256: string; bytes: number }> {
  await checkEngine(module);
  const root = path.resolve(outDir);
  await rm(root, { recursive: true, force: true });
  await mkdir(root, { recursive: true });
  const entry = path.join(root, "interpreter.wasm");
  await writeFile(entry, module);
  return {
    entry,
    sha256: createHash("sha256").update(module).digest("hex"),
    bytes: module.byteLength,
  };
}
