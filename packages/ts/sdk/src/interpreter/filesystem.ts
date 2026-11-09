/** A mounted capability has no access to host paths or other mounts. */
export interface MountedFilesystem {
  readText(path: string): Promise<string>;
  writeText(path: string, value: string): Promise<void>;
  dispose(): void | Promise<void>;
}
export interface FilesystemProvider {
  /** Open independent permission and lifetime scopes over explicitly shared storage. */
  open(mount: string, writable: boolean): MountedFilesystem;
}

const TEXT_LIMIT = 1 << 20;
const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

/** Enforce the portable text limit before data crosses a provider or guest seam. */
export function boundedText(value: string): string {
  if (typeof value !== "string") throw new Error("Filesystem text must be a string");
  if (
    value.length > TEXT_LIMIT ||
    (value.length > Math.floor(TEXT_LIMIT / 3) && encoder.encode(value).byteLength > TEXT_LIMIT)
  )
    throw new Error("Text exceeds the one mebibyte limit");
  return value;
}

/** Reject traversal before any provider receives a relative file name. */
function parts(path: string): string[] {
  const result = path.split("/");
  if (
    path.includes("\\") ||
    path.includes("\0") ||
    result.some((part) => !part || part === "." || part === "..")
  )
    throw new Error("Filesystem paths must be normalized relative paths");
  return result;
}
function bounded(bytes: Uint8Array): Uint8Array {
  if (bytes.byteLength > TEXT_LIMIT) throw new Error("Text exceeds the one mebibyte limit");
  return bytes;
}

/** Bind a provider to one installation's permissions and physical lifetime. */
function scoped(
  writable: boolean,
  read: (path: string) => Promise<string>,
  write: (path: string, value: string) => Promise<void>,
): MountedFilesystem {
  let closed = false;
  const check = (path: string) => {
    if (closed) throw new Error("Filesystem mount is closed");
    parts(path);
  };
  return {
    async readText(path) {
      check(path);
      return boundedText(await read(path));
    },
    async writeText(path, value) {
      check(path);
      if (!writable) throw new Error("Filesystem mount is read-only");
      await write(path, boundedText(value));
    },
    dispose() {
      closed = true;
    },
  };
}

/** Supply shared ephemeral storage without granting any Orb access automatically. */
export function memoryFilesystem(
  files: Readonly<Record<string, string | Uint8Array>> = {},
): FilesystemProvider {
  const data = new Map<string, Uint8Array>();
  for (const [path, value] of Object.entries(files)) {
    parts(path);
    data.set(path, typeof value === "string" ? encoder.encode(value) : value.slice());
  }
  for (const path of data.keys()) {
    const segments = parts(path);
    for (let end = 1; end < segments.length; end++)
      if (data.has(segments.slice(0, end).join("/")))
        throw new Error("Memory path overlaps a file");
  }
  return Object.freeze({
    open(_mount: string, writable: boolean) {
      return scoped(
        writable,
        async (path) => {
          const value = data.get(path);
          if (!value) throw new Error("File does not exist");
          return decoder.decode(bounded(value));
        },
        async (path, value) => {
          const segments = parts(path);
          for (let end = 1; end < segments.length; end++) {
            const parent = segments.slice(0, end).join("/");
            if (data.has(parent)) throw new Error("Parent path is a file");
            if (![...data.keys()].some((key) => key.startsWith(`${parent}/`)))
              throw new Error("Parent directory does not exist");
          }
          if ([...data.keys()].some((key) => key.startsWith(`${path}/`)))
            throw new Error("Path is a directory");
          data.set(path, bounded(encoder.encode(value)));
        },
      );
    },
  });
}

/** Resolve every operation from the directory handle supplied by the embedding host. */
export async function opfsFilesystem(
  directory: FileSystemDirectoryHandle,
): Promise<FilesystemProvider> {
  const file = async (path: string, create: boolean) => {
    const segments = parts(path);
    let parent = directory;
    for (const segment of segments.slice(0, -1)) parent = await parent.getDirectoryHandle(segment);
    return parent.getFileHandle(segments.at(-1)!, { create });
  };
  return Object.freeze({
    open(_mount: string, writable: boolean) {
      return scoped(
        writable,
        async (path) => {
          const value = await (await file(path, false)).getFile();
          if (value.size > TEXT_LIMIT) throw new Error("Text exceeds the one mebibyte limit");
          return decoder.decode(bounded(new Uint8Array(await value.arrayBuffer())));
        },
        async (path, value) => {
          const stream = await (await file(path, true)).createWritable();
          try {
            await stream.write(value);
            await stream.close();
          } catch (error) {
            await stream.abort().catch(() => {});
            throw error;
          }
        },
      );
    },
  });
}
