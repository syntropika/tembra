import * as filesystem from "@bytecodealliance/preview2-shim/filesystem";

interface Adapter {
  getRoot(capability: unknown): any;
  dispose?(): void | Promise<void>;
}
interface BrowserFilesystem {
  InMemoryFilesystemAdapter: new () => Adapter;
  OpfsFilesystemAdapter: new () => Adapter & { flush(): Promise<void> };
  loadOpfsCapability(handle: FileSystemDirectoryHandle): Promise<unknown>;
  createFilesystem(config: { adapter: Adapter; preopens: Record<string, unknown> }): {
    types: unknown;
    preopens: { getDirectories(): [unknown, string][] };
    dispose(): void | Promise<void>;
  };
}
const implementation = filesystem as unknown as BrowserFilesystem;
interface Entry {
  dir?: Record<string, Entry>;
  source?: string | Uint8Array;
}
export interface FilesystemProvider {
  /** Create isolated descriptor namespaces over the provider's explicitly shared data. */
  open(
    mount: string,
    writable: boolean,
  ): {
    types: unknown;
    preopens: { getDirectories(): [unknown, string][] };
    dispose(): void | Promise<void>;
  };
}

const mutations = new Set([
  "write",
  "writeViaStream",
  "appendViaStream",
  "setSize",
  "setTimes",
  "createDirectoryAt",
  "setTimesAt",
  "linkAt",
  "renameAt",
  "symlinkAt",
  "unlinkFileAt",
  "removeDirectoryAt",
]);
/** Restrict every descriptor, including descendants, before the standard shim can wrap it. */
export function readonlyDescriptor(descriptor: any): any {
  return new Proxy(descriptor, {
    get(target, key) {
      if (typeof key === "string" && mutations.has(key))
        return () => {
          throw "access";
        };
      if (key === "getFlags")
        return () => {
          const flags = target.getFlags();
          const restrict = (flags: any) => ({ ...flags, write: false, mutateDirectory: false });
          return flags instanceof Promise ? flags.then(restrict) : restrict(flags);
        };
      if (key === "openAt")
        return (pathFlags: unknown, path: string, openFlags: any, flags: any) => {
          if (openFlags.create || openFlags.truncate || flags.write || flags.mutateDirectory)
            throw "access";
          const child = target.openAt(pathFlags, path, openFlags, flags);
          return child instanceof Promise
            ? child.then(readonlyDescriptor)
            : readonlyDescriptor(child);
        };
      const value = Reflect.get(target, key, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}
/** Namespace ownership is per installation; disposing one Orb never disposes another Orb's adapter. */
function provider(create: () => Adapter, capability: unknown): FilesystemProvider {
  return Object.freeze({
    open(mount: string, writable: boolean) {
      const adapter = create();
      return implementation.createFilesystem({
        adapter: {
          getRoot: (capability) => {
            const descriptor = adapter.getRoot(capability);
            return writable ? descriptor : readonlyDescriptor(descriptor);
          },
          dispose: () => adapter.dispose?.(),
        },
        preopens: { [mount]: capability },
      });
    },
  });
}

/** Supply ephemeral shared storage without granting any Orb a mount automatically. */
export function memoryFilesystem(
  files: Readonly<Record<string, string | Uint8Array>> = {},
): FilesystemProvider {
  const root: Entry = { dir: {} };
  for (const [path, value] of Object.entries(files)) {
    const parts = path.split("/");
    if (parts.some((part) => !part || part === "." || part === ".."))
      throw new Error("Memory paths must be relative normalized paths");
    let directory = root;
    for (const part of parts.slice(0, -1)) {
      const entry = (directory.dir![part] ??= { dir: {} });
      if (!entry.dir) throw new Error("Memory path overlaps a file");
      directory = entry;
    }
    const name = parts.at(-1)!;
    if (directory.dir![name]) throw new Error("Duplicate memory path");
    directory.dir![name] = { source: typeof value === "string" ? value : value.slice() };
  }
  return provider(() => new implementation.InMemoryFilesystemAdapter(), root);
}

/** Bind only the directory handle supplied by the host; guest code never receives browser handles. */
export async function opfsFilesystem(
  directory: FileSystemDirectoryHandle,
): Promise<FilesystemProvider> {
  const capability = await implementation.loadOpfsCapability(directory);
  return provider(() => new implementation.OpfsFilesystemAdapter(), capability);
}
