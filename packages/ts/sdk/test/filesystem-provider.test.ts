import { expect, test } from "bun:test";
import {
  type Descriptor,
  type DirectoryDescriptor,
  type DirectoryEntry,
  type FileDescriptor,
  FilesystemError,
  type FilesystemErrorCode,
  type FilesystemProvider,
  type MountedFilesystem,
  memoryFilesystem,
  opfsFilesystem,
} from "../src/interpreter/filesystem.ts";

const text = new TextEncoder();
const bytes = (value: string) => text.encode(value);
const decode = (value: Uint8Array) => new TextDecoder().decode(value);

async function code(operation: Promise<unknown> | (() => unknown)): Promise<FilesystemErrorCode> {
  try {
    await (typeof operation === "function" ? operation() : operation);
  } catch (error) {
    if (error instanceof FilesystemError) return error.code;
    throw error;
  }
  throw new Error("Expected a FilesystemError");
}

async function file(directory: DirectoryDescriptor, path: string, write = false) {
  const opened = await directory.openAt(path, { write });
  expect(opened.type).toBe("file");
  return opened as FileDescriptor;
}

async function folder(directory: DirectoryDescriptor, path: string, write = false) {
  const opened = await directory.openAt(path, { directory: true, write });
  expect(opened.type).toBe("directory");
  return opened as DirectoryDescriptor;
}

async function list(directory: DirectoryDescriptor): Promise<DirectoryEntry[]> {
  const result: DirectoryEntry[] = [];
  for await (const entry of directory.readDirectory()) result.push(entry);
  return result.sort((a, b) => a.name.localeCompare(b.name));
}

async function readAll(descriptor: FileDescriptor): Promise<Uint8Array> {
  return descriptor.read(0n, Number((await descriptor.stat()).size));
}

test("memory bytes keep exact offsets, gaps, appends and bounded range reads", async () => {
  const root = memoryFilesystem({ "data.bin": new Uint8Array([0xff, 0xfe, 0x00, 0x80]) })
    .open("/data", true)
    .openRoot();
  const data = await file(root, "data.bin", true);
  expect([...(await data.read(1n, 2))]).toEqual([0xfe, 0x00]);
  expect((await data.read(3n, 1 << 20)).byteLength).toBe(1);
  expect((await data.read(99n, 16)).byteLength).toBe(0);
  expect((await data.read(2n ** 63n, 16)).byteLength).toBe(0);

  await data.write(6n, new Uint8Array([0xc3, 0x28]));
  expect([...(await readAll(data))]).toEqual([0xff, 0xfe, 0x00, 0x80, 0, 0, 0xc3, 0x28]);
  await data.append(new Uint8Array([7]));
  expect((await data.stat()).size).toBe(9n);

  await data.setSize(2n);
  await data.setSize(5n);
  expect([...(await readAll(data))]).toEqual([0xff, 0xfe, 0, 0, 0]);

  // Returned chunks are copies: later writes do not change them, and they do not change storage.
  const chunk = await data.read(0n, 2);
  await data.write(0n, new Uint8Array([1, 2]));
  chunk[0] = 9;
  expect([...chunk]).toEqual([9, 0xfe]);
  expect([...(await data.read(0n, 2))]).toEqual([1, 2]);

  expect(await code(data.read(-1n, 1))).toBe("invalid");
  expect(await code(data.read(0n, -1))).toBe("invalid");
  expect(await code(data.read(0n, 1.5))).toBe("invalid");
  expect(await code(data.write(2n ** 31n, new Uint8Array([1])))).toBe("file-too-large");
  expect(await code(data.setSize(2n ** 40n))).toBe("file-too-large");
  expect(await code(data.write(0n, [1, 2] as unknown as Uint8Array))).toBe("invalid");
});

test("memory directories enumerate lazily and mutate with precise errors", async () => {
  const root = memoryFilesystem({ "a/one.txt": "1", "a/b/two.txt": "2", "top.txt": "t" })
    .open("/m", true)
    .openRoot();
  expect(await list(root)).toEqual([
    { name: "a", type: "directory" },
    { name: "top.txt", type: "file" },
  ]);
  expect(await list(await folder(root, "a"))).toEqual([
    { name: "b", type: "directory" },
    { name: "one.txt", type: "file" },
  ]);

  await root.createDirectoryAt("a/new");
  expect((await root.statAt("a/new")).type).toBe("directory");
  expect(await code(root.createDirectoryAt("a/new"))).toBe("exist");
  expect(await code(root.createDirectoryAt("top.txt"))).toBe("exist");
  expect(await code(root.createDirectoryAt("."))).toBe("exist");
  expect(await code(root.createDirectoryAt("missing/x"))).toBe("no-entry");
  expect(await code(root.removeDirectoryAt("a"))).toBe("not-empty");
  expect(await code(root.removeDirectoryAt("top.txt"))).toBe("not-directory");
  expect(await code(root.removeDirectoryAt("missing"))).toBe("no-entry");
  expect(await code(root.removeDirectoryAt("."))).toBe("invalid");
  expect(await code(root.unlinkFileAt("a/b"))).toBe("is-directory");
  expect(await code(root.unlinkFileAt("top.txt/"))).toBe("not-directory");
  await root.removeDirectoryAt("a/new");
  await root.unlinkFileAt("a/b/two.txt");
  await root.removeDirectoryAt("a/b");
  expect(await code(root.statAt("a/b"))).toBe("no-entry");

  expect(await code(root.openAt("fresh.txt"))).toBe("no-entry");
  const fresh = (await root.openAt("fresh.txt", { create: true, write: true })) as FileDescriptor;
  await fresh.write(0n, bytes("abc"));
  expect(await code(root.openAt("fresh.txt", { create: true, exclusive: true, write: true }))).toBe(
    "exist",
  );
  const reopened = (await root.openAt("fresh.txt", {
    create: true,
    write: true,
  })) as FileDescriptor;
  expect(decode(await readAll(reopened))).toBe("abc");
  const truncated = (await root.openAt("fresh.txt", {
    truncate: true,
    write: true,
  })) as FileDescriptor;
  expect((await truncated.stat()).size).toBe(0n);
  expect(await code(root.openAt("fresh.txt", { truncate: true }))).toBe("invalid");
  expect(await code(root.openAt("x", { create: true, directory: true, write: true }))).toBe(
    "invalid",
  );
  expect(await code(root.openAt("a", { truncate: true, write: true }))).toBe("is-directory");
  expect(await code(root.openAt("top.txt", { directory: true }))).toBe("not-directory");

  // A listing is lazy over live storage and stops with bad-descriptor once its descriptor closes.
  const listing = root.readDirectory();
  expect((await listing.next()).done).toBe(false);
  root.close();
  expect(await code(listing.next())).toBe("bad-descriptor");
  const second = memoryFilesystem({ "x/y": "1" }).open("/m", false).openRoot().readDirectory();
  expect((await second.next()).value).toEqual({ name: "x", type: "directory" });
  expect(await second.return?.()).toEqual({ done: true, value: undefined });
  expect((await second.next()).done).toBe(true);
});

test("paths resolve from the opened directory and never escape it", async () => {
  const root = memoryFilesystem({ "outer.txt": "o", "a/inner.txt": "i", "a/b/deep.txt": "d" })
    .open("/m", true)
    .openRoot();
  const a = await folder(root, "a", true);
  expect(decode(await readAll(await file(a, "b/../inner.txt")))).toBe("i");
  expect(decode(await readAll(await file(a, "./b//deep.txt")))).toBe("d");
  expect(decode(await readAll(await file(root, "a/../outer.txt")))).toBe("o");
  expect((await a.statAt(".")).type).toBe("directory");
  expect((await a.statAt("b/..")).type).toBe("directory");

  expect(await code(a.statAt(".."))).toBe("not-permitted");
  expect(await code(a.openAt("../outer.txt"))).toBe("not-permitted");
  expect(await code(a.openAt("b/../../outer.txt"))).toBe("not-permitted");
  expect(await code(a.createDirectoryAt("../escape"))).toBe("not-permitted");
  expect(await code(a.renameAt("inner.txt", a, "../stolen.txt"))).toBe("not-permitted");
  expect(await code(root.statAt("/etc/passwd"))).toBe("not-permitted");
  expect(await code(root.statAt(""))).toBe("no-entry");
  expect(await code(root.statAt("outer.txt\0x"))).toBe("invalid");
  expect(await code(root.statAt("outer.txt/"))).toBe("not-directory");
  expect(await code(root.statAt("outer.txt/x"))).toBe("not-directory");
  expect(await code(root.statAt("outer.txt/.."))).toBe("not-directory");
  expect(await list(root)).toEqual([
    { name: "a", type: "directory" },
    { name: "outer.txt", type: "file" },
  ]);
});

test("opened files and directories keep identity through rename and unlink", async () => {
  const root = memoryFilesystem({ "a/b/file.txt": "before" }).open("/m", true).openRoot();
  const b = await folder(root, "a/b", true);
  const opened = await file(root, "a/b/file.txt", true);
  await root.createDirectoryAt("z");

  await root.renameAt("a/b/file.txt", root, "z/moved.txt");
  await opened.write(0n, bytes("after!"));
  expect(decode(await readAll(await file(root, "z/moved.txt")))).toBe("after!");
  expect(await code(root.statAt("a/b/file.txt"))).toBe("no-entry");

  // Renaming an ancestor does not change paths resolved from an open descriptor.
  await root.renameAt("a", root, "renamed");
  await b.createDirectoryAt("child");
  expect((await root.statAt("renamed/b/child")).type).toBe("directory");
  expect(await b.isSameObject(await folder(root, "renamed/b"))).toBe(true);
  expect(await b.isSameObject(root)).toBe(false);

  // An unlinked file stays usable through its descriptor and is not reachable by path.
  await root.unlinkFileAt("z/moved.txt");
  await opened.append(bytes("+"));
  expect(decode(await readAll(opened))).toBe("after!+");
  expect(await code(root.statAt("z/moved.txt"))).toBe("no-entry");
  expect(await list(await folder(root, "z"))).toEqual([]);

  // A removed directory keeps its descriptor but accepts no new entries.
  await b.removeDirectoryAt("child");
  await root.removeDirectoryAt("renamed/b");
  expect((await b.stat()).type).toBe("directory");
  expect(await code(b.createDirectoryAt("again"))).toBe("no-entry");
  expect(await code(b.openAt("new.txt", { create: true, write: true }))).toBe("no-entry");
});

test("rename follows POSIX replacement rules and verifies storage identity", async () => {
  const storage = memoryFilesystem({
    "src.txt": "source",
    "dst.txt": "old",
    "dir/inside.txt": "x",
    "empty/.keep": "",
    "full/item": "y",
  });
  const root = storage.open("/m", true).openRoot();
  const replaced = await file(root, "dst.txt");
  await root.unlinkFileAt("empty/.keep");

  await root.renameAt("src.txt", root, "dst.txt");
  expect(decode(await readAll(await file(root, "dst.txt")))).toBe("source");
  expect(decode(await readAll(replaced))).toBe("old");
  expect(await code(root.statAt("src.txt"))).toBe("no-entry");

  expect(await code(root.renameAt("missing", root, "x"))).toBe("no-entry");
  expect(await code(root.renameAt("dst.txt", root, "dir"))).toBe("is-directory");
  expect(await code(root.renameAt("dir", root, "dst.txt"))).toBe("not-directory");
  expect(await code(root.renameAt("dir", root, "full"))).toBe("not-empty");
  expect(await code(root.renameAt("dir", root, "dir/nested"))).toBe("invalid");
  expect(await code(root.renameAt(".", root, "x"))).toBe("invalid");
  expect(await code(root.renameAt("dst.txt", root, "missing/x"))).toBe("no-entry");
  await root.renameAt("dst.txt", root, "dst.txt");
  await root.renameAt("dir", root, "empty");
  expect(decode(await readAll(await file(root, "empty/inside.txt")))).toBe("x");

  // Separately scoped mounts of the same storage can rename between their descriptors.
  const other = storage.open("/other", true).openRoot();
  await root.renameAt("dst.txt", await folder(other, "full", true), "moved.txt");
  expect((await root.statAt("full/moved.txt")).size).toBe(6n);

  // Another storage is cross-device; nothing moves.
  const foreign = memoryFilesystem().open("/m", true).openRoot();
  expect(await code(root.renameAt("full/moved.txt", foreign, "x"))).toBe("cross-device");
  const imposter = { ...foreign } as DirectoryDescriptor;
  expect(await code(root.renameAt("full/moved.txt", imposter, "x"))).toBe("cross-device");
  expect(await list(foreign)).toEqual([]);

  const readonly = storage.open("/ro", false).openRoot();
  expect(await code(root.renameAt("full/moved.txt", readonly, "x"))).toBe("not-permitted");
  const closed = storage.open("/c", true).openRoot();
  closed.close();
  expect(await code(root.renameAt("full/moved.txt", closed, "x"))).toBe("bad-descriptor");
  expect((await root.statAt("full/moved.txt")).type).toBe("file");
});

test("scopes have independent rights and deterministic disposal over shared storage", async () => {
  const storage = memoryFilesystem({ "shared.txt": "v1" });
  const writer = storage.open("/w", true);
  const reader = storage.open("/r", false);
  const writable = writer.openRoot();
  const readable = reader.openRoot();
  expect(writer.writable).toBe(true);
  expect(readable.writable).toBe(false);

  for (const refused of [
    readable.openAt("shared.txt", { write: true }),
    readable.openAt("new.txt", { create: true }),
    readable.openAt("shared.txt", { truncate: true, write: true }),
    readable.createDirectoryAt("d"),
    readable.removeDirectoryAt("d"),
    readable.unlinkFileAt("shared.txt"),
    readable.renameAt("shared.txt", readable, "x"),
    readable.renameAt("shared.txt", writable, "x"),
  ])
    expect(await code(refused)).toBe("not-permitted");
  expect(await list(writable)).toEqual([{ name: "shared.txt", type: "file" }]);

  // A read-only descriptor stays read-only even in a writable scope.
  const plain = await file(writable, "shared.txt");
  expect(plain.writable).toBe(false);
  expect(await code(plain.write(0n, bytes("x")))).toBe("not-permitted");
  expect(await code(plain.append(bytes("x")))).toBe("not-permitted");
  expect(await code(plain.setSize(0n))).toBe("not-permitted");
  const readOnlyDirectory = await folder(writable, ".");
  expect(await code(readOnlyDirectory.createDirectoryAt("d"))).toBe("not-permitted");

  const editable = await file(writable, "shared.txt", true);
  const observed = await file(readable, "shared.txt");
  await editable.write(0n, bytes("v2"));
  expect(decode(await readAll(observed))).toBe("v2");

  await writer.dispose();
  for (const closed of [editable.read(0n, 1), writable.statAt("shared.txt"), editable.sync()])
    expect(await code(closed)).toBe("bad-descriptor");
  expect(await code(() => writer.openRoot())).toBe("bad-descriptor");
  expect(decode(await readAll(observed))).toBe("v2");

  observed.close();
  observed.close();
  expect(await code(observed.stat())).toBe("bad-descriptor");
  expect((await readable.statAt("shared.txt")).size).toBe(2n);
  await reader.dispose();
  await reader.dispose();
});

test("memory seed paths must be normalized and must not overlap", () => {
  expect(() => memoryFilesystem({ "../x": "1" })).toThrow("normalized");
  expect(() => memoryFilesystem({ "a//b": "1" })).toThrow("normalized");
  expect(() => memoryFilesystem({ a: "1", "a/b": "2" })).toThrow("overlaps");
  const seed = new Uint8Array([1, 2]);
  const root = memoryFilesystem({ seed }).open("/m", true).openRoot();
  seed[0] = 9;
  return root
    .openAt("seed")
    .then((opened) => (opened as FileDescriptor).read(0n, 2))
    .then((value) => expect([...value]).toEqual([1, 2]));
});

// A minimal in-process stand-in for OPFS handles. It is not Chrome: it checks this provider's use
// of the handle API (fresh slices, keepExistingData writers, move, entries), not browser behavior.

type FakeNode = FakeFileNode | FakeDirectoryNode;
interface FakeFileNode {
  kind: "file";
  data: Uint8Array;
  modified: number;
}
interface FakeDirectoryNode {
  kind: "directory";
  children: Map<string, FakeNode>;
}

const domError = (name: string) => new DOMException(name, name);

class FakeOpfs {
  readonly calls: string[] = [];
  writers = 0;
  maxWriters = 0;
  delay = 0;
  /** Fail the next writer `write` with a quota error, as a full origin would. */
  failNextWrite = false;
  constructor(readonly supportsMove: boolean) {}

  root(): FileSystemDirectoryHandle {
    return this.handle({ kind: "directory", children: new Map() }, "", undefined);
  }

  /** Locate the current parent by identity so moved handles keep working. */
  parentOf(node: FakeNode, from: FakeDirectoryNode): FakeDirectoryNode | undefined {
    for (const child of from.children.values()) {
      if (child === node) return from;
      if (child.kind === "directory") {
        const found = this.parentOf(node, child);
        if (found) return found;
      }
    }
    return undefined;
  }

  handle(node: FakeNode, name: string, top: FakeDirectoryNode | undefined): any {
    const fake = this;
    const root = top ?? (node as FakeDirectoryNode);
    const wait = () => new Promise((resolve) => setTimeout(resolve, fake.delay));
    const base: any = {
      kind: node.kind,
      name,
      node,
      async isSameEntry(other: { node?: FakeNode }) {
        return other.node === node;
      },
    };
    if (this.supportsMove)
      base.move = async (destination: { node: FakeDirectoryNode }, target: string) => {
        fake.calls.push(`move:${target}`);
        const parent = fake.parentOf(node, root);
        if (!parent) throw domError("NotFoundError");
        const existing = destination.node.children.get(target);
        if (existing && (existing.kind !== "file" || node.kind !== "file"))
          throw domError("InvalidModificationError");
        for (const [key, value] of parent.children) if (value === node) parent.children.delete(key);
        destination.node.children.set(target, node);
        base.name = target;
      };
    if (node.kind === "file")
      return Object.assign(base, {
        async getFile() {
          fake.calls.push("getFile");
          if (!fake.parentOf(node, root)) throw domError("NotFoundError");
          return new File([node.data.slice()], name, { lastModified: node.modified });
        },
        async createWritable(options: { keepExistingData?: boolean }) {
          fake.calls.push(`createWritable:${options.keepExistingData}`);
          let swap = options.keepExistingData ? node.data.slice() : new Uint8Array(0);
          fake.maxWriters = Math.max(fake.maxWriters, ++fake.writers);
          const resize = (size: number) => {
            const next = new Uint8Array(size);
            next.set(swap.subarray(0, Math.min(size, swap.length)));
            swap = next;
          };
          return {
            async write(chunk: { type: "write"; position: number; data: Uint8Array }) {
              await wait();
              if (fake.failNextWrite) {
                fake.failNextWrite = false;
                throw domError("QuotaExceededError");
              }
              const end = chunk.position + chunk.data.byteLength;
              if (end > swap.length) resize(end);
              swap.set(chunk.data, chunk.position);
            },
            async truncate(size: number) {
              resize(size);
            },
            async close() {
              await wait();
              node.data = swap;
              node.modified = Date.now();
              fake.writers--;
            },
            async abort() {
              fake.writers--;
            },
          };
        },
      });
    const directory = node;
    const child = (child: string, kind: FakeNode["kind"], create?: boolean) => {
      if (child.includes("\\")) throw new TypeError("Invalid name");
      let found = directory.children.get(child);
      if (!found && create) {
        found =
          kind === "file"
            ? { kind, data: new Uint8Array(0), modified: Date.now() }
            : { kind, children: new Map() };
        directory.children.set(child, found);
      }
      if (!found) throw domError("NotFoundError");
      if (found.kind !== kind) throw domError("TypeMismatchError");
      return fake.handle(found, child, root);
    };
    return Object.assign(base, {
      async getFileHandle(name: string, options?: { create?: boolean }) {
        return child(name, "file", options?.create);
      },
      async getDirectoryHandle(name: string, options?: { create?: boolean }) {
        return child(name, "directory", options?.create);
      },
      async removeEntry(name: string) {
        const found = directory.children.get(name);
        if (!found) throw domError("NotFoundError");
        if (found.kind === "directory" && found.children.size)
          throw domError("InvalidModificationError");
        directory.children.delete(name);
      },
      async resolve(possible: { node: FakeNode }) {
        const walk = (from: FakeDirectoryNode, path: string[]): string[] | null => {
          if (from === possible.node) return path;
          for (const [key, value] of from.children)
            if (value.kind === "directory") {
              const found = walk(value, [...path, key]);
              if (found) return found;
            }
          return null;
        };
        return walk(directory, []);
      },
      async *entries() {
        for (const [key, value] of directory.children)
          yield [key, fake.handle(value, key, root)] as const;
      },
    });
  }
}

async function opfs(supportsMove = true) {
  const fake = new FakeOpfs(supportsMove);
  const provider = await opfsFilesystem(fake.root());
  return { fake, provider, root: provider.open("/o", true).openRoot() };
}

test("injected OPFS-shaped handles: fresh range reads and keepExistingData writers", async () => {
  const { fake, root } = await opfs();
  const created = (await root.openAt("blob.bin", { create: true, write: true })) as FileDescriptor;
  await created.write(0n, new Uint8Array([0xff, 0x00, 0xfe]));
  await created.write(5n, new Uint8Array([1]));
  await created.append(new Uint8Array([2]));
  fake.calls.length = 0;
  expect([...(await created.read(1n, 4))]).toEqual([0x00, 0xfe, 0, 0]);
  expect(fake.calls).toEqual(["getFile"]);
  expect([...(await created.read(0n, 100))]).toEqual([0xff, 0, 0xfe, 0, 0, 1, 2]);
  expect((await created.read(50n, 1)).byteLength).toBe(0);

  fake.calls.length = 0;
  await created.setSize(2n);
  expect(fake.calls).toEqual(["createWritable:true"]);
  expect((await created.stat()).size).toBe(2n);
  expect(typeof (await created.stat()).modified).toBe("number");

  // Concurrent writes and appends through two descriptors are serialized per entry.
  const other = (await root.openAt("blob.bin", { write: true })) as FileDescriptor;
  fake.delay = 2;
  await Promise.all([
    created.write(0n, bytes("AA")),
    other.write(2n, bytes("BB")),
    created.append(bytes("C")),
    other.append(bytes("D")),
  ]);
  fake.delay = 0;
  expect(fake.maxWriters).toBe(1);
  expect(decode(await created.read(0n, 10))).toBe("AABBCD");
  await created.sync();

  const truncated = (await root.openAt("blob.bin", {
    truncate: true,
    write: true,
  })) as FileDescriptor;
  expect(fake.calls.at(-1)).toBe("createWritable:true");
  expect((await truncated.stat()).size).toBe(0n);
});

test("injected OPFS-shaped handles: directories, rename and precise unsupported", async () => {
  const { fake, provider, root } = await opfs();
  await root.createDirectoryAt("dir");
  await root.createDirectoryAt("dir/sub");
  const moving = (await root.openAt("dir/a.txt", { create: true, write: true })) as FileDescriptor;
  await moving.write(0n, bytes("payload"));
  expect(await list(root)).toEqual([{ name: "dir", type: "directory" }]);
  expect(await list(await folder(root, "dir"))).toEqual([
    { name: "a.txt", type: "file" },
    { name: "sub", type: "directory" },
  ]);
  expect(await code(root.removeDirectoryAt("dir"))).toBe("not-empty");
  expect(await code(root.unlinkFileAt("dir"))).toBe("is-directory");
  expect(await code(root.removeDirectoryAt("dir/a.txt"))).toBe("not-directory");
  expect(await code(root.statAt("dir/missing"))).toBe("no-entry");
  expect(await code(root.statAt("dir/a.txt/x"))).toBe("not-directory");
  expect(await code(root.openAt("bad\\name", { create: true, write: true }))).toBe("invalid");
  expect(await code(root.createDirectoryAt("dir"))).toBe("exist");

  // The descriptor's shared handle is the one moved, so it follows the rename.
  await root.renameAt("dir/a.txt", root, "b.txt");
  await moving.append(bytes("!"));
  expect(decode(await readAll(await file(root, "b.txt")))).toBe("payload!");
  expect(await code(root.renameAt("dir", root, "dir/sub/inner"))).toBe("invalid");
  expect(await code(root.renameAt("b.txt", root, "dir"))).toBe("is-directory");
  await root.createDirectoryAt("other");
  fake.calls.length = 0;
  expect(await code(root.renameAt("dir/sub", root, "other"))).toBe("unsupported");
  expect(fake.calls.filter((call) => call.startsWith("move"))).toEqual([]);

  // Unlink: OPFS keeps no detached storage, so the open descriptor reports no-entry.
  await root.unlinkFileAt("b.txt");
  expect(await code(moving.read(0n, 1))).toBe("no-entry");

  const foreign = memoryFilesystem().open("/m", true).openRoot();
  expect(await code(root.renameAt("dir", foreign, "x"))).toBe("cross-device");
  const readonly = provider.open("/ro", false).openRoot();
  expect(await code(readonly.createDirectoryAt("x"))).toBe("not-permitted");
  expect(await code(readonly.openAt("dir/new", { create: true, write: true }))).toBe(
    "not-permitted",
  );
  expect(await list(readonly)).toEqual([
    { name: "dir", type: "directory" },
    { name: "other", type: "directory" },
  ]);

  const legacy = await opfs(false);
  await legacy.root.openAt("x", { create: true, write: true });
  expect(await code(legacy.root.renameAt("x", legacy.root, "y"))).toBe("unsupported");
  expect((await legacy.root.statAt("x")).type).toBe("file");
  expect(await code(legacy.root.statAt("y"))).toBe("no-entry");
});

test("injected OPFS-shaped dispose closes descriptors and waits for in-flight writes", async () => {
  const { fake, provider } = await opfs();
  const scope = provider.open("/o", true);
  const root = scope.openRoot();
  const target = (await root.openAt("slow.bin", { create: true, write: true })) as FileDescriptor;
  fake.delay = 10;
  let settled = false;
  const write = target.write(0n, bytes("done")).then(() => (settled = true));
  const disposal = scope.dispose();
  expect(await code(target.read(0n, 1))).toBe("bad-descriptor");
  await disposal;
  expect(settled).toBe(true);
  await write;
  fake.delay = 0;
  const reader = provider.open("/o", false).openRoot();
  expect(decode(await readAll(await file(reader, "slow.bin")))).toBe("done");
});

test("custom injected providers implement the same contract without browser globals", async () => {
  // Wrap memory storage with a counter; the interfaces alone define the contract.
  let opened = 0;
  const inner = memoryFilesystem({ "x.txt": "x" });
  const custom: FilesystemProvider = {
    open(mount, writable): MountedFilesystem {
      const scope = inner.open(mount, writable);
      return {
        writable,
        openRoot: () => {
          opened++;
          return scope.openRoot();
        },
        dispose: () => scope.dispose(),
      };
    },
  };
  const mount = custom.open("/c", false);
  const root = mount.openRoot();
  const entry: Descriptor = await root.openAt("x.txt");
  expect(entry.type).toBe("file");
  expect(opened).toBe(1);
  expect(await code(root.unlinkFileAt("x.txt"))).toBe("not-permitted");
  await mount.dispose();
  expect(await code(entry.stat())).toBe("bad-descriptor");
});

test("injected OPFS-shaped opens share one writer queue and survive disposal races", async () => {
  const { fake, provider, root } = await opfs();
  (await root.openAt("f", { create: true, write: true })).close();
  const [a, b] = (await Promise.all([
    root.openAt("f", { write: true }),
    root.openAt("f", { write: true }),
  ])) as FileDescriptor[];
  fake.delay = 2;
  fake.maxWriters = 0;
  await Promise.all([a!.write(0n, bytes("12")), b!.write(2n, bytes("34"))]);
  expect(fake.maxWriters).toBe(1);
  expect(decode(await a!.read(0n, 4))).toBe("1234");

  // An open finishing after its scope is disposed fails without releasing others' registrations.
  const scope = provider.open("/x", true);
  const pending = scope.openRoot().openAt("f", { write: true });
  void scope.dispose();
  expect(await code(pending)).toBe("bad-descriptor");
  a!.close();
  const c = (await root.openAt("f", { write: true })) as FileDescriptor;
  fake.maxWriters = 0;
  await Promise.all([b!.write(0n, bytes("ab")), c.write(2n, bytes("cd"))]);
  fake.delay = 0;
  expect(fake.maxWriters).toBe(1);
  expect(decode(await c.read(0n, 4))).toBe("abcd");
});

/** A chunk source that records whether it was started and closed, optionally failing. */
function source(
  chunks: Uint8Array[],
  options: { fail?: Error; onYield?: (index: number) => unknown } = {},
) {
  const state = { started: false, returned: false, pulled: 0 };
  const iterable: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]() {
      state.started = true;
      return {
        async next() {
          if (state.pulled > 0) await options.onYield?.(state.pulled - 1);
          if (state.pulled === chunks.length) {
            if (options.fail) throw options.fail;
            return { done: true, value: undefined };
          }
          return { done: false, value: chunks[state.pulled++]! };
        },
        async return() {
          state.returned = true;
          return { done: true, value: undefined };
        },
      };
    },
  };
  return { iterable, state };
}

test("memory writeStream writes positioned and appended chunks, visible as written", async () => {
  const root = memoryFilesystem({ f: "ab" }).open("/m", true).openRoot();
  const target = await file(root, "f", true);
  const sizes: bigint[] = [];
  const positioned = source([bytes("12"), new Uint8Array(0), bytes("34")], {
    onYield: async () => sizes.push((await target.stat()).size),
  });
  await target.writeStream!(positioned.iterable, 4n);
  expect(decode(await readAll(target))).toBe("ab\0\x001234");
  expect(sizes).toEqual([6n, 6n, 8n]);
  await target.writeStream!(source([bytes("X"), bytes("Y")]).iterable);
  expect(decode(await readAll(target))).toBe("ab\0\x001234XY");

  // Admission failures never start the caller's iterator.
  const readOnly = await file(root, "f");
  const refused = source([bytes("no")]);
  expect(await code(readOnly.writeStream!(refused.iterable))).toBe("not-permitted");
  expect(await code(target.writeStream!(refused.iterable, -1n))).toBe("invalid");
  expect(await code(target.writeStream!(refused.iterable, 2n ** 31n))).toBe("file-too-large");
  expect(refused.state.started).toBe(false);

  // Accepted chunks stay; the producer error is rethrown unchanged and the iterator is closed.
  const cancelled = new Error("cancelled by caller");
  const failing = source([bytes("ok")], { fail: cancelled });
  await target.setSize(0n);
  expect(await target.writeStream!(failing.iterable, 0n).catch((error) => error)).toBe(cancelled);
  expect(decode(await readAll(target))).toBe("ok");
  const invalid = source([bytes("v"), [1] as unknown as Uint8Array, bytes("never")]);
  expect(await code(target.writeStream!(invalid.iterable, 0n))).toBe("invalid");
  expect(invalid.state).toEqual({ started: true, returned: true, pulled: 2 });
  expect(decode(await readAll(target))).toBe("vk");

  // Closing the descriptor stops the stream at the next chunk, keeping what was written.
  const closing = source([bytes("1"), bytes("2"), bytes("3")], {
    onYield: (index) => index === 0 && target.close(),
  });
  expect(await code(target.writeStream!(closing.iterable, 0n))).toBe("bad-descriptor");
  expect(closing.state.returned).toBe(true);
  expect(decode(await readAll(await file(root, "f")))).toBe("1k");
});

test("injected OPFS-shaped writeStream uses one writer, commits once and hides uncommitted bytes", async () => {
  const { fake, root } = await opfs();
  const target = (await root.openAt("big", { create: true, write: true })) as FileDescriptor;
  await target.write(0n, bytes("old!"));
  const observer = await file(root, "big");
  const other = await file(root, "big", true);
  const seen: string[] = [];
  let otherWrite: Promise<void> | undefined;
  const chunks = Array.from({ length: 8 }, (_, index) => bytes(String(index)));
  fake.calls.length = 0;
  const stream = source(chunks, {
    onYield: async (index) => {
      if (index === 0) otherWrite = other.write(0n, bytes("Z"));
      seen.push(`${decode(await observer.read(0n, 16))}:${(await observer.stat()).size}`);
    },
  });
  await target.writeStream!(stream.iterable, 2n);
  expect(fake.calls.filter((call) => call.startsWith("createWritable"))).toEqual([
    "createWritable:true",
    "createWritable:true",
  ]);
  // Readers saw only committed content while the stream ran; the other writer queued behind it.
  expect(new Set(seen)).toEqual(new Set(["old!:4"]));
  await otherWrite;
  expect(decode(await readAll(observer))).toBe("Zl01234567");

  // Append mode starts at the committed end; an empty stream opens no writer.
  fake.calls.length = 0;
  await target.writeStream!(source([]).iterable);
  expect(fake.calls).toEqual([]);
  await target.writeStream!(source([bytes("+"), bytes("-")]).iterable);
  expect(decode(await readAll(observer))).toBe("Zl01234567+-");

  // A producer failure still commits accepted chunks.
  const failure = new Error("pump aborted");
  const failing = source([bytes("AB")], { fail: failure });
  expect(await target.writeStream!(failing.iterable, 0n).catch((error) => error)).toBe(failure);
  expect(decode(await observer.read(0n, 2))).toBe("AB");

  // A platform failure aborts the writer: nothing of this stream is committed.
  fake.failNextWrite = false;
  const quota = source([bytes("QQ"), bytes("RR")], {
    onYield: () => (fake.failNextWrite = true),
  });
  expect(await code(target.writeStream!(quota.iterable, 0n))).toBe("insufficient-space");
  expect(quota.state.returned).toBe(true);
  expect(decode(await readAll(observer))).toBe("AB01234567+-");
  expect(fake.writers).toBe(0);
});
