/** A cache identity for JCO glue, plus each original core module in traversal order. */
export interface ComponentLayout {
  readonly source: Uint8Array;
  readonly identity: Uint8Array;
  readonly modules: readonly Uint8Array[];
}

/** Read section framing only; the caller has already verified the actual component in Rust. */
export function componentLayout(component: Uint8Array): ComponentLayout | undefined {
  const source = component.slice();
  const chunks: Uint8Array[] = [];
  const modules: Uint8Array[] = [];
  let length = 0;
  let sections = 0;
  const append = (bytes: Uint8Array) => {
    chunks.push(bytes);
    length += bytes.length;
  };
  const integer = (value: number): Uint8Array => {
    const bytes: number[] = [];
    do {
      const byte = value % 128;
      value = Math.floor(value / 128);
      bytes.push(byte | (value ? 128 : 0));
    } while (value);
    return Uint8Array.from(bytes);
  };
  const read = (bytes: Uint8Array, offset: number): [number, number] | undefined => {
    let value = 0;
    for (let shift = 0; shift < 35; shift += 7) {
      const byte = bytes[offset++];
      if (byte === undefined || (shift === 28 && byte > 15)) return;
      value += (byte & 127) * 2 ** shift;
      if (!(byte & 128)) return [value, offset];
    }
  };
  const metadata = (bytes: Uint8Array) => {
    const size = read(bytes, 0);
    if (!size || size[0] > 32 || size[1] + size[0] > bytes.length) return false;
    const name = new TextDecoder().decode(bytes.subarray(size[1], size[1] + size[0]));
    return name === "tembra:orb" || name === "tembra:runtime";
  };
  const visit = (bytes: Uint8Array, isComponent: boolean, depth: number): boolean => {
    const header = isComponent ? [0, 97, 115, 109, 13, 0, 1, 0] : [0, 97, 115, 109, 1, 0, 0, 0];
    if (depth > 32 || header.some((byte, index) => bytes[index] !== byte)) return false;
    append(bytes.subarray(0, 8));
    let offset = 8;
    while (offset < bytes.length) {
      if (++sections > 16384) return false;
      const id = bytes[offset++]!;
      const size = read(bytes, offset);
      if (!size || size[1] + size[0] > bytes.length || id > (isComponent ? 11 : 13)) return false;
      const body = bytes.subarray(size[1], size[1] + size[0]);
      offset = size[1] + size[0];
      if ((!isComponent && id === 11) || (isComponent && id === 0 && metadata(body))) continue;
      append(Uint8Array.of(id));
      if (isComponent && (id === 1 || id === 4)) {
        if (id === 1) modules.push(body);
        if (!visit(body, id === 4, depth + 1)) return false;
      } else {
        append(integer(body.length));
        append(body);
      }
    }
    // This is an unambiguous, mode-aware cache key, not a replacement executable component.
    append(Uint8Array.of(255));
    return true;
  };
  if (!visit(source, true, 0) || modules.length === 0) return;
  const identity = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    identity.set(chunk, offset);
    offset += chunk.length;
  }
  return { source, identity, modules };
}

/** Hash bytes computed by this host; artifacts never choose cache identities. */
export async function contentHash(bytes: Uint8Array): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes as BufferSource));
  return Array.from(digest, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

export interface FileTemplate {
  readonly files: readonly (readonly [string, Uint8Array | number])[];
  readonly moduleCount: number;
}

const coreName = (index: number) => `orb.core${index === 0 ? "" : index + 1}.wasm`;

/** Reuse glue only when every original module has one exact, unambiguous generated file. */
export async function fileTemplate(
  files: readonly (readonly [string, Uint8Array])[],
  modules: readonly Uint8Array[],
): Promise<FileTemplate | undefined> {
  const indices = new Map<string, number>();
  for (const [index, bytes] of modules.entries()) {
    const hash = await contentHash(bytes);
    if (indices.has(hash)) return;
    indices.set(hash, index);
  }
  const mapped = new Set<number>();
  const template: (readonly [string, Uint8Array | number])[] = [];
  for (const [name, bytes] of files) {
    const index = name.endsWith(".wasm") ? indices.get(await contentHash(bytes)) : undefined;
    if (index !== undefined) {
      if (mapped.has(index) || name !== coreName(index)) return;
      mapped.add(index);
    } else {
      const core = /^orb\.core([0-9]*)\.wasm$/.exec(name);
      if (core) {
        const ordinal = core[1] === "" ? 0 : Number(core[1]) - 1;
        if (ordinal < modules.length || name !== coreName(ordinal)) return;
      }
    }
    template.push([name, index ?? bytes]);
  }
  if (mapped.size !== modules.length) return;
  return { files: template, moduleCount: modules.length };
}

/** Fill each module position with this Orb's bytes, including its own data and initialization. */
export function instantiateFiles(
  template: FileTemplate,
  modules: readonly Uint8Array[],
): Map<string, Uint8Array> {
  if (modules.length !== template.moduleCount)
    throw new Error("Component template module count differs");
  return new Map(
    template.files.map(([name, bytes]) => {
      if (typeof bytes !== "number") return [name, bytes] as const;
      const module = modules[bytes];
      if (!module || name !== coreName(bytes))
        throw new Error("Invalid component template module position");
      return [name, module] as const;
    }),
  );
}
