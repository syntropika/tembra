import {
  type Descriptor,
  type DirectoryDescriptor,
  type DirectoryEntry,
  type EntryType,
  type FileDescriptor,
  type FileStat,
  type FilesystemProvider,
  type MountedFilesystem,
  type OpenOptions,
  FilesystemError,
  fail,
} from "./contract.ts";

/**
 * Node-level primitives of one storage. Names are single, already confined components; the
 * scope layer owns rights, lifetime, path resolution and argument validation.
 */
export interface Store<N> {
  readonly root: N;
  /** Largest file size in bytes. */
  readonly limit: number;
  type(node: N): EntryType;
  /** An existing child, trying `expected` first where lookups are type-specific. */
  lookup(parent: N, name: string, expected?: EntryType): Promise<N>;
  /** Create a new child; `exist` when any entry already has the name. */
  create(parent: N, name: string, type: EntryType): Promise<N>;
  /** Canonical node retained by a descriptor until `release`. */
  retain(node: N): Promise<N>;
  release(node: N): void;
  stat(node: N): Promise<FileStat>;
  same(a: N, b: N): Promise<boolean>;
  entries(directory: N): AsyncIterator<DirectoryEntry>;
  read(file: N, offset: number, length: number): Promise<Uint8Array>;
  /** Write at `offset`, or append when it is undefined. */
  write(file: N, offset: number | undefined, data: Uint8Array): Promise<void>;
  resize(file: N, size: number): Promise<void>;
  /**
   * Optional one-writer stream at `offset` (append when undefined). The chunks are validated,
   * non-empty and re-check the descriptor; without it, chunks go through `write` one by one.
   */
  writeStream?(
    file: N,
    offset: number | undefined,
    chunks: AsyncIterable<Uint8Array>,
  ): Promise<void>;
  sync(node: N): Promise<void>;
  remove(parent: N, name: string, type: EntryType): Promise<void>;
  rename(from: N, fromName: string, to: N, toName: string): Promise<void>;
}

/** Per-descriptor state; the descriptor object itself exposes only the public interface. */
interface Opened<N> {
  readonly scope: Scope<N>;
  readonly node: N;
  readonly writable: boolean;
  readonly retained: boolean;
  closed: boolean;
}

const owners = new WeakMap<object, Opened<unknown>>();

/** One `provider.open` result: rights, descriptor ownership and in-flight settlement. */
class Scope<N> implements MountedFilesystem {
  #closed = false;
  readonly #opened = new Set<Opened<N>>();
  readonly #pending = new Set<Promise<unknown>>();

  constructor(
    readonly store: Store<N>,
    readonly writable: boolean,
  ) {}

  openRoot(): DirectoryDescriptor {
    if (this.#closed) fail("bad-descriptor", "Filesystem mount is closed");
    return this.adopt(this.store.root, this.writable, false) as DirectoryDescriptor;
  }

  async dispose(): Promise<void> {
    if (!this.#closed) {
      this.#closed = true;
      for (const opened of this.#opened) this.close(opened);
    }
    await Promise.allSettled([...this.#pending]);
  }

  /** Wrap a node in a descriptor; on a scope closed meanwhile, the caller releases the node. */
  adopt(node: N, writable: boolean, retained: boolean): Descriptor {
    if (this.#closed) fail("bad-descriptor", "Filesystem mount is closed");
    const opened: Opened<N> = { scope: this, node, writable, retained, closed: false };
    this.#opened.add(opened);
    const descriptor =
      this.store.type(node) === "file" ? fileDescriptor(opened) : directoryDescriptor(opened);
    owners.set(descriptor, opened as Opened<unknown>);
    return descriptor;
  }

  close(opened: Opened<N>): void {
    if (opened.closed) return;
    opened.closed = true;
    this.#opened.delete(opened);
    if (opened.retained) this.store.release(opened.node);
  }

  /** Check liveness and rights before any backend effect, then track the call until it settles. */
  run<T>(
    opened: Opened<N>,
    mutate: boolean,
    operation: (store: Store<N>, node: N) => Promise<T>,
  ): Promise<T> {
    let promise: Promise<T>;
    try {
      live(opened);
      if (mutate && !opened.writable) fail("not-permitted", "Descriptor is read-only");
      promise = operation(this.store, opened.node);
    } catch (error) {
      return Promise.reject(error);
    }
    this.#pending.add(promise);
    const forget = () => this.#pending.delete(promise);
    promise.then(forget, forget);
    return promise;
  }
}

function live(opened: Opened<unknown>): void {
  if (opened.closed) fail("bad-descriptor", "Descriptor is closed");
}

/** Validate a non-negative `filesize` inside the storage limit. */
function size(value: bigint, limit: number, extra = 0): number {
  if (typeof value !== "bigint" || value < 0n) fail("invalid", "Invalid file position");
  if (value + BigInt(extra) > BigInt(limit)) fail("file-too-large", "File size limit exceeded");
  return Number(value);
}

function bytes(data: Uint8Array): Uint8Array {
  if (!(data instanceof Uint8Array)) fail("invalid", "File data must be a Uint8Array");
  return data;
}

interface Resolved<N> {
  /** Directory holding `name`, or the named directory itself when `name` is absent. */
  readonly parent: N;
  readonly name?: string;
  /** A trailing `/` requires a directory. */
  readonly directory: boolean;
}

/** Resolve component by component from the descriptor's own node; `..` never leaves it. */
async function resolve<N>(store: Store<N>, base: N, path: string): Promise<Resolved<N>> {
  if (typeof path !== "string" || path.includes("\0")) fail("invalid", "Invalid path");
  if (path === "") fail("no-entry", "Empty path");
  if (path.startsWith("/")) fail("not-permitted", "Absolute paths are not permitted");
  const parts = path.split("/").filter((part) => part !== "" && part !== ".");
  const stack = [base];
  const directory = path.endsWith("/");
  for (const [index, part] of parts.entries()) {
    if (part === "..") {
      if (stack.length === 1) fail("not-permitted", "Path escapes the opened directory");
      stack.pop();
    } else if (index === parts.length - 1) {
      return { parent: stack.at(-1)!, name: part, directory };
    } else {
      const child = await store.lookup(stack.at(-1)!, part, "directory");
      if (store.type(child) !== "directory") fail("not-directory", "Path component is a file");
      stack.push(child);
    }
  }
  return { parent: stack.at(-1)!, directory };
}

/** Resolve an existing entry, honoring a trailing `/`. */
async function existing<N>(store: Store<N>, base: N, path: string): Promise<N> {
  const target = await resolve(store, base, path);
  if (target.name === undefined) return target.parent;
  const node = await store.lookup(target.parent, target.name);
  if (target.directory && store.type(node) !== "directory")
    fail("not-directory", "Path is not a directory");
  return node;
}

/** Resolve a path that must name an entry inside a directory rather than the base itself. */
async function named<N>(
  store: Store<N>,
  base: N,
  path: string,
): Promise<Resolved<N> & { name: string }> {
  const target = await resolve(store, base, path);
  if (target.name === undefined) fail("invalid", "Path names the opened directory itself");
  return target as Resolved<N> & { name: string };
}

function descriptorBase<N>(opened: Opened<N>) {
  const { scope } = opened;
  return {
    writable: opened.writable,
    stat: () => scope.run(opened, false, (store, node) => store.stat(node)),
    isSameObject: (other: Descriptor) =>
      scope.run(opened, false, async (store, node) => {
        const target = owners.get(other) as Opened<N> | undefined;
        if (!target || target.scope.store !== store) return false;
        live(target);
        return store.same(node, target.node);
      }),
    sync: () => scope.run(opened, false, (store, node) => store.sync(node)),
    close: () => scope.close(opened),
  };
}

function fileDescriptor<N>(opened: Opened<N>): FileDescriptor {
  const { scope } = opened;
  return Object.freeze({
    type: "file" as const,
    ...descriptorBase(opened),
    read: (offset: bigint, length: number) =>
      scope.run(opened, false, async (store, node) => {
        if (
          typeof offset !== "bigint" ||
          offset < 0n ||
          !Number.isSafeInteger(length) ||
          length < 0
        )
          fail("invalid", "Invalid file range");
        if (length === 0 || offset > BigInt(store.limit)) return new Uint8Array(0);
        return store.read(node, Number(offset), length);
      }),
    write: (offset: bigint, data: Uint8Array) =>
      scope.run(opened, true, async (store, node) => {
        const position = size(offset, store.limit, bytes(data).byteLength);
        if (data.byteLength) await store.write(node, position, data);
      }),
    append: (data: Uint8Array) =>
      scope.run(opened, true, async (store, node) => {
        if (bytes(data).byteLength) await store.write(node, undefined, data);
      }),
    setSize: (value: bigint) =>
      scope.run(opened, true, (store, node) => store.resize(node, size(value, store.limit))),
    writeStream: (chunks: AsyncIterable<Uint8Array>, offset?: bigint) =>
      scope.run(opened, true, async (store, node) => {
        let position = offset === undefined ? undefined : size(offset, store.limit);
        // Lazy: `chunks` is untouched until the store pulls, i.e. after admission succeeded.
        const checked = (async function* () {
          for await (const chunk of chunks) {
            live(opened);
            if (bytes(chunk).byteLength) yield chunk;
          }
        })();
        if (store.writeStream) return store.writeStream(node, position, checked);
        for await (const chunk of checked) {
          if (position !== undefined && position + chunk.byteLength > store.limit)
            fail("file-too-large", "File size limit exceeded");
          await store.write(node, position, chunk);
          if (position !== undefined) position += chunk.byteLength;
        }
      }),
  });
}

function directoryDescriptor<N>(opened: Opened<N>): DirectoryDescriptor {
  const { scope } = opened;
  const base = descriptorBase(opened);
  return Object.freeze({
    type: "directory" as const,
    ...base,
    openAt: (path: string, options: OpenOptions = {}) => {
      const { create = false, exclusive = false, truncate = false, write = false } = options;
      return scope.run(opened, create || truncate || write, async (store, node) => {
        if (truncate && !write) fail("invalid", "Truncation requires write access");
        if (create && options.directory) fail("invalid", "Use createDirectoryAt for directories");
        const target = await resolve(store, node, path);
        const directory = options.directory === true || target.directory;
        let child: N;
        if (target.name === undefined) {
          if (create && exclusive) fail("exist", "Path names an existing directory");
          child = target.parent;
        } else if (create && exclusive) {
          child = await store.create(target.parent, target.name, "file");
        } else {
          child = await openExisting(store, target.parent, target.name, create, directory);
        }
        const type = store.type(child);
        if (directory && type !== "directory") fail("not-directory", "Path is not a directory");
        if (truncate && type === "directory") fail("is-directory", "Cannot truncate a directory");
        const retained = await store.retain(child);
        try {
          if (truncate) await store.resize(retained, 0);
          return scope.adopt(retained, write, true);
        } catch (error) {
          store.release(retained);
          throw error;
        }
      });
    },
    statAt: (path: string) =>
      scope.run(opened, false, async (store, node) =>
        store.stat(await existing(store, node, path)),
      ),
    readDirectory: () => entries(opened),
    createDirectoryAt: (path: string) =>
      scope.run(opened, true, async (store, node) => {
        const target = await resolve(store, node, path);
        if (target.name === undefined) fail("exist", "Directory already exists");
        await store.create(target.parent, target.name, "directory");
      }),
    removeDirectoryAt: (path: string) =>
      scope.run(opened, true, async (store, node) => {
        const target = await named(store, node, path);
        await store.remove(target.parent, target.name, "directory");
      }),
    unlinkFileAt: (path: string) =>
      scope.run(opened, true, async (store, node) => {
        const target = await named(store, node, path);
        if (target.directory) {
          const child = await store.lookup(target.parent, target.name);
          fail(store.type(child) === "file" ? "not-directory" : "is-directory", "Not a file path");
        }
        await store.remove(target.parent, target.name, "file");
      }),
    renameAt: (oldPath: string, newDirectory: DirectoryDescriptor, newPath: string) =>
      scope.run(opened, true, async (store, node) => {
        const target = owners.get(newDirectory) as Opened<N> | undefined;
        if (!target || target.scope.store !== store)
          fail("cross-device", "Rename target belongs to another storage");
        live(target);
        if (store.type(target.node) !== "directory") fail("not-directory", "Target is a file");
        if (!target.writable) fail("not-permitted", "Target directory is read-only");
        const from = await named(store, node, oldPath);
        const to = await named(store, target.node, newPath);
        await store.rename(from.parent, from.name, to.parent, to.name);
      }),
  });
}

/** Open an existing entry, creating a file when requested and absent. */
async function openExisting<N>(
  store: Store<N>,
  parent: N,
  name: string,
  create: boolean,
  directory: boolean,
): Promise<N> {
  const expected = directory ? "directory" : "file";
  try {
    return await store.lookup(parent, name, expected);
  } catch (error) {
    if (!create || !(error instanceof FilesystemError) || error.code !== "no-entry") throw error;
  }
  try {
    return await store.create(parent, name, "file");
  } catch (error) {
    // Another call created it between lookup and creation; open what now exists.
    if (!(error instanceof FilesystemError) || error.code !== "exist") throw error;
    return store.lookup(parent, name, expected);
  }
}

/** A lazy listing whose every step re-checks the descriptor and is tracked by its scope. */
function entries<N>(opened: Opened<N>): AsyncIterableIterator<DirectoryEntry> {
  let source: AsyncIterator<DirectoryEntry> | undefined;
  let done = false;
  const finished = { done: true, value: undefined } as const;
  const iterator: AsyncIterableIterator<DirectoryEntry> = {
    next() {
      if (done) return Promise.resolve(finished);
      return opened.scope.run(opened, false, async (store, node) => {
        source ??= store.entries(node);
        const step = await source.next();
        if (step.done) done = true;
        return step;
      });
    },
    async return() {
      done = true;
      await source?.return?.();
      return finished;
    },
    [Symbol.asyncIterator]() {
      return iterator;
    },
  };
  return iterator;
}

/** Expose one storage as a provider whose every `open` is an independent scope. */
export function provider<N>(store: Store<N>): FilesystemProvider {
  return Object.freeze({
    open: (_mount: string, writable: boolean): MountedFilesystem => new Scope(store, writable),
  });
}
