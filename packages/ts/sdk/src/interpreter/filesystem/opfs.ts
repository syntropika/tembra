import {
  type EntryType,
  type FileStat,
  type FilesystemErrorCode,
  type FilesystemProvider,
  FilesystemError,
  fail,
} from "./contract.ts";
import { type Store, provider } from "./scope.ts";

// OPFS storage: direct handle operations, fresh range reads and per-entry writer queues.

type OpfsHandle = FileSystemFileHandle | FileSystemDirectoryHandle;

/** Members some engines lack or that the bundled DOM types omit; each use is feature-checked. */
interface OpfsExtensions {
  move?(destination: FileSystemDirectoryHandle, name: string): Promise<void>;
  entries?(): AsyncIterableIterator<[string, OpfsHandle]>;
}

const DOM_ERRORS: Readonly<Record<string, FilesystemErrorCode>> = {
  NotFoundError: "no-entry",
  TypeMismatchError: "not-directory",
  InvalidModificationError: "not-empty",
  NotAllowedError: "access",
  SecurityError: "access",
  QuotaExceededError: "insufficient-space",
  NoModificationAllowedError: "busy",
  NotSupportedError: "unsupported",
  TypeError: "invalid",
};

/** Map a platform failure to a WASI code without retaining its object graph. */
function platformError(
  error: unknown,
  overrides: Readonly<Record<string, FilesystemErrorCode>> = {},
): never {
  if (error instanceof FilesystemError) throw error;
  const name = String((error as { name?: unknown })?.name);
  const message = error instanceof Error ? error.message : String(error);
  throw new FilesystemError(overrides[name] ?? DOM_ERRORS[name] ?? "io", message);
}

function opfsStore(root: FileSystemDirectoryHandle): Store<OpfsHandle> {
  /** Canonical handle objects shared by descriptors, grouped by name to bound comparisons. */
  const retained = new Map<string, { handle: OpfsHandle; count: number }[]>();
  /** Tail of the provider's serialized writer operations per canonical file handle. */
  const queues = new WeakMap<OpfsHandle, Promise<unknown>>();
  let registry: Promise<unknown> = Promise.resolve();

  const guard = async <T>(
    operation: () => Promise<T>,
    overrides?: Readonly<Record<string, FilesystemErrorCode>>,
  ): Promise<T> => {
    try {
      return await operation();
    } catch (error) {
      return platformError(error, overrides);
    }
  };
  const serialize = <T>(handle: OpfsHandle, operation: () => Promise<T>): Promise<T> => {
    const result = (queues.get(handle) ?? Promise.resolve()).then(operation);
    queues.set(
      handle,
      result.catch(() => {}),
    );
    return result;
  };
  const canonical = async (handle: OpfsHandle) => {
    for (const entry of retained.get(handle.name) ?? [])
      if (await entry.handle.isSameEntry(handle)) return entry;
    return undefined;
  };
  const optional = async (parent: FileSystemDirectoryHandle, name: string) => {
    try {
      return await lookup(parent, name);
    } catch (error) {
      if (error instanceof FilesystemError && error.code === "no-entry") return undefined;
      throw error;
    }
  };
  const lookup = (parent: OpfsHandle, name: string, expected: EntryType = "file") =>
    guard(async (): Promise<OpfsHandle> => {
      const directory = parent as FileSystemDirectoryHandle;
      const first = () =>
        expected === "file" ? directory.getFileHandle(name) : directory.getDirectoryHandle(name);
      const second = () =>
        expected === "file" ? directory.getDirectoryHandle(name) : directory.getFileHandle(name);
      try {
        return await first();
      } catch (error) {
        if ((error as { name?: unknown })?.name !== "TypeMismatchError") throw error;
        return second();
      }
    });
  /** One positioned writer session; the swap file is committed only by a successful close. */
  const writeAt = (
    handle: FileSystemFileHandle,
    edit: (writer: FileSystemWritableFileStream) => Promise<void>,
  ) =>
    guard(async () => {
      const writer = await handle.createWritable({ keepExistingData: true });
      try {
        await edit(writer);
        await writer.close();
      } catch (error) {
        await writer.abort().catch(() => {});
        throw error;
      }
    });

  return {
    root,
    limit: Number.MAX_SAFE_INTEGER,
    type: (node) => (node.kind === "file" ? "file" : "directory"),
    lookup,
    async create(parent, name, type) {
      const directory = parent as FileSystemDirectoryHandle;
      // Check-then-create: exclusive within this provider, not across other tabs.
      if (await optional(directory, name)) fail("exist", "Entry already exists");
      return guard<OpfsHandle>(() =>
        type === "file"
          ? directory.getFileHandle(name, { create: true })
          : directory.getDirectoryHandle(name, { create: true }),
      );
    },
    // Serialized so concurrent opens of one entry cannot register two canonical handles.
    retain(node) {
      const result = registry.then(async () => {
        const entry = await canonical(node);
        if (entry) {
          entry.count++;
          return entry.handle;
        }
        retained.set(node.name, [...(retained.get(node.name) ?? []), { handle: node, count: 1 }]);
        return node;
      });
      registry = result.catch(() => {});
      return result;
    },
    release(node) {
      const entry = retained.get(node.name)?.find((candidate) => candidate.handle === node);
      if (!entry || --entry.count) return;
      // Keep the handle canonical until its queued writes settle, so a later opener queues behind.
      void (queues.get(node) ?? Promise.resolve()).then(() => {
        const group = retained.get(node.name);
        if (entry.count || !group?.includes(entry)) return;
        group.splice(group.indexOf(entry), 1);
        if (!group.length) retained.delete(node.name);
      });
    },
    stat: (node) =>
      guard(async (): Promise<FileStat> => {
        if (node.kind !== "file") return { type: "directory", size: 0n };
        const value = await node.getFile();
        return { type: "file", size: BigInt(value.size), modified: value.lastModified };
      }),
    same: (a, b) => guard(() => a.isSameEntry(b)),
    async *entries(node) {
      const iterate = (node as OpfsExtensions).entries;
      if (typeof iterate !== "function")
        fail("unsupported", "Directory enumeration is unavailable");
      const source = iterate.call(node);
      try {
        while (true) {
          const step = await guard(() => source.next());
          if (step.done) return;
          const [name, handle] = step.value;
          yield { name, type: handle.kind === "file" ? "file" : "directory" };
        }
      } finally {
        await source.return?.();
      }
    },
    // Each read slices a fresh snapshot, so memory stays bounded by the requested range.
    read: (node, offset, length) =>
      guard(async () => {
        const value = await (node as FileSystemFileHandle).getFile();
        if (offset >= value.size) return new Uint8Array(0);
        return new Uint8Array(await value.slice(offset, offset + length).arrayBuffer());
      }),
    write: (node, offset, data) =>
      serialize(node, async () => {
        const handle = node as FileSystemFileHandle;
        const position = offset ?? (await guard(() => handle.getFile())).size;
        if (position + data.byteLength > Number.MAX_SAFE_INTEGER)
          fail("file-too-large", "File size limit exceeded");
        await writeAt(handle, (writer) =>
          writer.write({ type: "write", position, data: data as Uint8Array<ArrayBuffer> }),
        );
      }),
    // One writer per stream: per-chunk createWritable copies the whole file each time (quadratic).
    // The entry queue is held until commit, so the swap file is never raced by provider writers.
    writeStream: (node, offset, chunks) =>
      serialize(node, async () => {
        const handle = node as FileSystemFileHandle;
        let position = offset;
        let writer: FileSystemWritableFileStream | undefined;
        let failure: { error: unknown } | undefined;
        try {
          for await (const chunk of chunks) {
            position ??= (await guard(() => handle.getFile())).size;
            if (position + chunk.byteLength > Number.MAX_SAFE_INTEGER)
              fail("file-too-large", "File size limit exceeded");
            writer ??= await guard(() => handle.createWritable({ keepExistingData: true }));
            const active = writer;
            const at = position;
            await guard(() =>
              active.write({ type: "write", position: at, data: chunk as Uint8Array<ArrayBuffer> }),
            ).catch(async (error: unknown) => {
              // A failed writer cannot commit; discard this stream's uncommitted bytes.
              writer = undefined;
              await active.abort().catch(() => {});
              throw error;
            });
            position += chunk.byteLength;
          }
        } catch (error) {
          failure = { error };
        }
        // Accepted bytes are committed even when the producer failed or the descriptor closed.
        if (writer) {
          const active = writer;
          await guard(() => active.close()).catch(async (error: unknown) => {
            await active.abort().catch(() => {});
            throw error;
          });
        }
        if (failure) throw failure.error;
      }),
    resize: (node, size) =>
      serialize(node, () =>
        writeAt(node as FileSystemFileHandle, (writer) => writer.truncate(size)),
      ),
    // Writes commit when their writer closes; sync waits for this provider's queued writes.
    sync: async (node) => {
      await queues.get(node);
    },
    async remove(parent, name, type) {
      const directory = parent as FileSystemDirectoryHandle;
      const node = await lookup(directory, name, type);
      if (node.kind !== (type === "file" ? "file" : "directory"))
        fail(type === "file" ? "is-directory" : "not-directory", "Entry has another type");
      await guard(() => directory.removeEntry(name));
    },
    async rename(fromNode, fromName, toNode, toName) {
      const from = fromNode as FileSystemDirectoryHandle;
      const to = toNode as FileSystemDirectoryHandle;
      const source = await lookup(from, fromName);
      const move = (source as OpfsExtensions).move;
      if (typeof move !== "function") fail("unsupported", "This browser cannot move OPFS entries");
      const replaced = await optional(to, toName);
      if (replaced) {
        if (await guard(() => replaced.isSameEntry(source))) return;
        if (source.kind === "file" && replaced.kind === "directory")
          fail("is-directory", "Target is a directory");
        if (source.kind === "directory" && replaced.kind === "file")
          fail("not-directory", "Target is not a directory");
        if (replaced.kind === "directory") {
          const children = (replaced as OpfsExtensions).entries?.call(replaced);
          const first = children ? await guard(() => children.next()) : undefined;
          await children?.return?.();
          if (first && !first.done) fail("not-empty", "Target directory is not empty");
          fail("unsupported", "Replacing a directory cannot be done atomically in OPFS");
        }
      }
      if (source.kind === "directory") {
        const inside = await guard(() => (source as FileSystemDirectoryHandle).resolve(to));
        if (inside) fail("invalid", "Cannot move a directory into itself");
      }
      // Move the shared handle so descriptors of this entry follow it, after its queued writes.
      const entry = await canonical(source);
      const handle = entry?.handle ?? source;
      const previous = handle.name;
      await serialize(handle, () =>
        guard(() => move.call(handle, to, toName), { InvalidModificationError: "unsupported" }),
      );
      // The moved handle reports its new name; regroup it so later lookups still find it.
      if (entry && handle.name !== previous) {
        const group = retained.get(previous) ?? [];
        group.splice(group.indexOf(entry), 1);
        if (!group.length) retained.delete(previous);
        retained.set(handle.name, [...(retained.get(handle.name) ?? []), entry]);
      }
    },
  };
}

/** Resolve every operation from the directory handle supplied by the embedding host. */
export async function opfsFilesystem(
  directory: FileSystemDirectoryHandle,
): Promise<FilesystemProvider> {
  return provider(opfsStore(directory));
}
