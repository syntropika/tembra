/**
 * Browser implementations of the generated `wasi:filesystem` methods over provider descriptors
 * (`filesystem.ts`). Tembra checks descriptor kind, rights and lexical confinement before any
 * provider call, then providers enforce their own scope as well. Error codes follow upstream
 * Wasmtime: a wrong descriptor kind or a missing file right is `bad-descriptor` (`not-directory`
 * for directory methods), and a directory mutation without `mutate-directory` is
 * `not-permitted`.
 *
 * Browser providers have no settable timestamps, hard links or symbolic links: those methods
 * report `unsupported` after their authority checks. `advise` is an advisory no-op, and
 * `set-times`/`set-times-at` with `no-change` for both timestamps succeed without effect.
 */
import {
  FilesystemError,
  type Descriptor as ProviderDescriptor,
  type DirectoryDescriptor,
  type FileDescriptor,
  type FileStat,
  type MountedFilesystem,
} from "./filesystem.ts";
import {
  abortable,
  cancelled,
  HostFuture,
  type Implementation,
  type InputStream,
  OutputStream,
  type StandardCall,
  type StandardResource,
  type StandardServices,
} from "./standard-streams.ts";

const TYPES = "wasi:filesystem/types@0.3.0";
const PREOPENS = "wasi:filesystem/preopens@0.3.0";
const U64 = (1n << 64n) - 1n;

/** A mount granted to this revision. */
export interface StandardMount {
  readonly path: string;
  readonly writable: boolean;
  readonly filesystem: MountedFilesystem;
}

/** jco `descriptor-flags`. */
interface DescriptorFlags {
  read: boolean;
  write: boolean;
  fileIntegritySync: boolean;
  dataIntegritySync: boolean;
  requestedWriteSync: boolean;
  mutateDirectory: boolean;
}

type ErrorCode = { tag: string };
type Result = { tag: "ok"; val?: unknown } | { tag: "err"; val: ErrorCode };
type Timestamp = { tag: "no-change" | "now" | "timestamp"; val?: unknown };

const ok = (val?: unknown): Result => ({ tag: "ok", val });
const err = (code: string): Result => ({ tag: "err", val: { tag: code } });

/** A provider descriptor behind one host handle; streams and borrows retain it independently. */
export class HostDescriptor implements StandardResource {
  borrows = 0;
  #users = 1;

  constructor(
    readonly host: StandardServices,
    readonly provider: ProviderDescriptor,
    readonly flags: Readonly<DescriptorFlags>,
  ) {}

  get file(): FileDescriptor | undefined {
    return this.provider.type === "file" ? this.provider : undefined;
  }

  get directory(): DirectoryDescriptor | undefined {
    return this.provider.type === "directory" ? this.provider : undefined;
  }

  /** `mutate-directory` for a directory. */
  get mutable(): boolean {
    return this.provider.type === "directory" && this.flags.mutateDirectory;
  }

  retain(): void {
    this.#users++;
  }

  release(): void {
    if (--this.#users === 0) closeProvider(this.host, this.provider);
  }
}

/**
 * Close a provider descriptor. The contract's `close()` is synchronous, but a custom provider
 * may still return a promise; it stays physically owned by the revision until it settles.
 */
function closeProvider(host: StandardServices, descriptor: ProviderDescriptor): void {
  host.cleanup(() => descriptor.close() as unknown);
}

/** Map a provider failure to its WASI error code; anything else is `io`. */
function code(error: unknown): string {
  return error instanceof FilesystemError ? error.code : "io";
}

/** A refused provider call is an error-code result unless the callback was cancelled. */
function failed(call: StandardCall, error: unknown): Result {
  if (call.signal?.aborted) throw cancelled();
  return err(code(error));
}

/**
 * Run a provider operation under physical accounting. The call itself rejects promptly on
 * cancellation; `orphan` releases a value that a cancelled call will never deliver.
 */
function provider<T>(
  call: StandardCall,
  operation: () => Promise<T>,
  orphan?: (value: T) => void,
): Promise<T> {
  const work = call.host.physical(call.signal, operation);
  if (orphan)
    work.then(
      (value) => {
        if (call.signal?.aborted) orphan(value);
      },
      () => {},
    );
  return abortable(work, call.signal);
}

/** Refuse a path lexically before any provider call; `undefined` admits it to the provider. */
function refusal(path: string): string | undefined {
  if (path.includes("\0")) return "invalid";
  if (path.startsWith("/")) return "not-permitted";
  let depth = 0;
  for (const part of path.split("/")) {
    if (part === "" || part === ".") continue;
    if (part !== "..") depth++;
    else if (--depth < 0) return "not-permitted";
  }
  return undefined;
}

/** Milliseconds since the epoch as a WASI `instant` with signed seconds. */
function instant(milliseconds: number | undefined) {
  if (milliseconds === undefined || !Number.isFinite(milliseconds)) return undefined;
  const whole = Math.floor(milliseconds);
  const seconds = Math.floor(whole / 1000);
  return { seconds: BigInt(seconds), nanoseconds: (whole - seconds * 1000) * 1_000_000 };
}

function descriptorType(type: "file" | "directory"): ErrorCode {
  return { tag: type === "file" ? "regular-file" : "directory" };
}

function descriptorStat(stat: FileStat) {
  return {
    type: descriptorType(stat.type),
    linkCount: 1n,
    size: stat.size,
    dataAccessTimestamp: undefined,
    dataModificationTimestamp: instant(stat.modified),
    statusChangeTimestamp: undefined,
  };
}

/** FNV-1a over the metadata browser providers report; equal metadata gives an equal hash. */
function metadataHash(stat: FileStat) {
  const hash = (seed: bigint) => {
    let value = seed;
    const mix = (word: bigint) => {
      for (let shift = 0n; shift < 64n; shift += 8n) {
        value ^= (word >> shift) & 0xffn;
        value = (value * 0x100000001b3n) & U64;
      }
    };
    mix(stat.type === "file" ? 1n : 2n);
    mix(BigInt.asUintN(64, stat.size));
    mix(stat.modified === undefined ? U64 : BigInt.asUintN(64, BigInt(Math.floor(stat.modified))));
    return value;
  };
  return { lower: hash(0xcbf29ce484222325n), upper: hash(0x84222325cbf29ce4n) };
}

/** Directory methods: the descriptor kind, then the lexical path, before the provider. */
function directoryAt(self: HostDescriptor, ...paths: string[]): DirectoryDescriptor | Result {
  const directory = self.directory;
  if (!directory) return err("not-directory");
  for (const path of paths) {
    const refused = refusal(path);
    if (refused) return err(refused);
  }
  return directory;
}

/** Directory mutations additionally need `mutate-directory` on every directory involved. */
function mutableAt(
  self: HostDescriptor,
  paths: string[],
  other?: HostDescriptor,
): DirectoryDescriptor | Result {
  if (!self.directory || (other && !other.directory)) return err("not-directory");
  if (!self.mutable || (other && !other.mutable)) return err("not-permitted");
  return directoryAt(self, ...paths);
}

const isResult = (value: unknown): value is Result =>
  typeof value === "object" && value !== null && "tag" in value;

/** Run one checked provider call and map its failure. */
async function attempt<T>(
  call: StandardCall,
  checked: T | Result,
  operation: (target: T) => Promise<unknown>,
): Promise<Result> {
  if (isResult(checked)) return checked;
  try {
    return ok(await provider(call, () => operation(checked)));
  } catch (error) {
    return failed(call, error);
  }
}

/** A sync stream method's failure is reported through its future, as upstream does. */
function failedOutput(failure: string): [OutputStream, HostFuture] {
  // The stream ends at its first read; the future carries the error.
  const stream = new OutputStream({ next: async () => undefined, close() {} }, () => {});
  return [stream, HostFuture.of(err(failure))];
}

/** Pair an output stream with the future that reports how it ended. */
function output(
  self: HostDescriptor,
  producer: { next(limit: number): Promise<unknown>; close?(): void },
): [OutputStream, HostFuture] {
  let settle!: (value: Result) => void;
  const value = new Promise<Result>((resolve) => {
    settle = resolve;
  });
  // The stream keeps its descriptor's provider object open after the handle is dropped.
  self.retain();
  const stream = new OutputStream(
    {
      next: producer.next as never,
      close() {
        producer.close?.();
        self.release();
      },
    },
    (failure) => settle(failure === undefined ? ok() : err(code(failure))),
  );
  return [stream, new HostFuture(value)];
}

/**
 * Present a transferred input stream as the provider's chunk iterable: chunks of at most
 * `STREAM_CHUNK` bytes, pulled only when the provider asks (backpressure), ending at the
 * writer's EOF. The provider's `return()` (early exit) stops the input; a stopped input ends
 * the provider's iteration with an error rather than a silent end of stream.
 */
function chunks(input: InputStream): AsyncIterable<Uint8Array> {
  return {
    [Symbol.asyncIterator]: () => ({
      async next(): Promise<IteratorResult<Uint8Array>> {
        const chunk = await input.read();
        if (chunk) return { done: false, value: chunk };
        if (input.cancelled) throw new Error("stream reader was dropped");
        return { done: true, value: undefined };
      },
      async return(): Promise<IteratorResult<Uint8Array>> {
        input.cancel();
        return { done: true, value: undefined };
      },
    }),
  };
}

/**
 * Consume a transferred input stream into a file; the future reports the outcome. A provider
 * with `writeStream` receives the whole stream in one call (one OPFS writer and commit);
 * otherwise each chunk is a sequential `write`/`append`. Either way the descriptor stays open
 * and the work stays physically owned until the provider settles; the input ends when its
 * writer is dropped, which the host forces at callback end and retirement.
 */
function consume(
  host: StandardServices,
  self: HostDescriptor,
  input: InputStream,
  offset: bigint | undefined,
): HostFuture {
  const file = self.file;
  if (!file || !self.flags.write) {
    input.cancel();
    return HostFuture.of(err("bad-descriptor"));
  }
  self.retain();
  const value = (async (): Promise<Result> => {
    try {
      if (file.writeStream) {
        const stream = input;
        await host.physical(undefined, () => file.writeStream!(chunks(stream), offset));
        return ok();
      }
      let position = offset;
      for (let chunk = await input.read(); chunk; chunk = await input.read()) {
        const bytes = chunk;
        if (position === undefined) await host.physical(undefined, () => file.append(bytes));
        else {
          const at = position;
          await host.physical(undefined, () => file.write(at, bytes));
          position += BigInt(bytes.byteLength);
        }
      }
      return ok();
    } catch (error) {
      return err(code(error));
    } finally {
      // Stop the writer side (later writes fail) and free any buffered budget.
      input.cancel();
      self.release();
    }
  })();
  host.cleanup(() => value);
  return new HostFuture(value, () => input.cancel());
}

/** Build the filesystem implementations over this revision's granted mounts. */
export function filesystemMethods(
  mounts: () => readonly StandardMount[],
): Readonly<Record<string, Implementation>> {
  const methods: Record<string, Implementation> = {
    [`${PREOPENS}#getDirectories`]: (call) =>
      // Like upstream, every call issues fresh owned descriptors for the preopened mounts.
      mounts().map((mount) => [
        new HostDescriptor(call.host, mount.filesystem.openRoot(), {
          read: true,
          write: false,
          fileIntegritySync: false,
          dataIntegritySync: false,
          requestedWriteSync: false,
          mutateDirectory: mount.writable,
        }),
        mount.path,
      ]),

    readViaStream(_call, [self, offset]: [HostDescriptor, bigint]) {
      const file = self.file;
      if (!file || !self.flags.read) return failedOutput("bad-descriptor");
      let position = offset;
      return output(self, {
        next: async (limit: number) => {
          const chunk = await _call.host.physical(undefined, () => file.read(position, limit));
          position += BigInt(chunk.byteLength);
          return chunk;
        },
      });
    },

    writeViaStream: ({ host }, [self, input, offset]: [HostDescriptor, InputStream, bigint]) =>
      consume(host, self, input, offset),

    appendViaStream: ({ host }, [self, input]: [HostDescriptor, InputStream]) =>
      consume(host, self, input, undefined),

    readDirectory({ host }, [self]: [HostDescriptor]) {
      const directory = self.directory;
      if (!directory) return failedOutput("not-directory");
      let entries: ReturnType<DirectoryDescriptor["readDirectory"]>;
      try {
        entries = directory.readDirectory();
      } catch (error) {
        return failedOutput(code(error));
      }
      let done = false;
      return output(self, {
        next: async () => {
          const next = await host.physical(undefined, () => entries.next());
          if (next.done) {
            done = true;
            return undefined;
          }
          return { type: descriptorType(next.value.type), name: next.value.name };
        },
        close() {
          // Close an unfinished provider iterator, also during retirement.
          if (!done) host.cleanup(() => entries.return?.());
        },
      });
    },

    async advise(_call, [self]: [HostDescriptor]) {
      return self.file ? ok() : err("bad-descriptor");
    },

    sync: (call, [self]: [HostDescriptor]) => attempt(call, self.provider, (p) => p.sync()),
    syncData: (call, [self]: [HostDescriptor]) => attempt(call, self.provider, (p) => p.sync()),

    async getFlags(_call, [self]: [HostDescriptor]) {
      return ok({ ...self.flags });
    },

    async getType(_call, [self]: [HostDescriptor]) {
      return ok(descriptorType(self.provider.type));
    },

    setSize(call, [self, size]: [HostDescriptor, bigint]) {
      const file = self.file && self.flags.write ? self.file : err("bad-descriptor");
      return attempt(call, file, (target) => target.setSize(size));
    },

    async setTimes(_call, [self, accessed, modified]: [HostDescriptor, Timestamp, Timestamp]) {
      if (self.file ? !self.flags.write : !self.mutable)
        return err(self.file ? "bad-descriptor" : "not-permitted");
      return accessed.tag === "no-change" && modified.tag === "no-change"
        ? ok()
        : err("unsupported");
    },

    setTimesAt(
      call,
      [self, , path, accessed, modified]: [HostDescriptor, unknown, string, Timestamp, Timestamp],
    ) {
      const checked = mutableAt(self, [path]);
      if (isResult(checked) || accessed.tag !== "no-change" || modified.tag !== "no-change")
        return Promise.resolve(isResult(checked) ? checked : err("unsupported"));
      // Nothing changes, but the entry must exist.
      return attempt(call, checked, (directory) => directory.statAt(path).then(() => undefined));
    },

    stat: (call, [self]: [HostDescriptor]) =>
      attempt(call, self.provider, (p) => p.stat().then(descriptorStat)),

    statAt: (call, [self, , path]: [HostDescriptor, unknown, string]) =>
      attempt(call, directoryAt(self, path), (directory) =>
        directory.statAt(path).then(descriptorStat),
      ),

    async openAt(
      call,
      [self, , path, open, flags]: [
        HostDescriptor,
        unknown,
        string,
        { create?: boolean; directory?: boolean; exclusive?: boolean; truncate?: boolean },
        Partial<DescriptorFlags>,
      ],
    ) {
      const write = Boolean(flags.write || flags.mutateDirectory);
      const directory = self.directory;
      if (!directory) return err("not-directory");
      // Write authority comes only from a mutable directory, checked before the provider.
      if ((open.create || open.truncate || write) && !self.mutable) return err("not-permitted");
      const refused = refusal(path);
      if (refused) return err(refused);
      try {
        const opened = await provider(
          call,
          () =>
            directory.openAt(path, {
              create: Boolean(open.create),
              exclusive: Boolean(open.exclusive),
              truncate: Boolean(open.truncate),
              directory: Boolean(open.directory),
              write,
            }),
          (descriptor) => closeProvider(call.host, descriptor),
        );
        const granted = opened.writable && write;
        return ok(
          new HostDescriptor(call.host, opened, {
            read: opened.type === "directory" || Boolean(flags.read),
            write: opened.type === "file" && granted,
            fileIntegritySync: Boolean(flags.fileIntegritySync),
            dataIntegritySync: Boolean(flags.dataIntegritySync),
            requestedWriteSync: Boolean(flags.requestedWriteSync),
            mutateDirectory: opened.type === "directory" && granted,
          }),
        );
      } catch (error) {
        return failed(call, error);
      }
    },

    async readlinkAt(call, [self, path]: [HostDescriptor, string]) {
      // There are no symbolic links: an existing entry is not one (`invalid`), as in POSIX.
      const result = await attempt(call, directoryAt(self, path), (directory) =>
        directory.statAt(path),
      );
      return result.tag === "ok" ? err("invalid") : result;
    },

    createDirectoryAt: (call, [self, path]: [HostDescriptor, string]) =>
      attempt(call, mutableAt(self, [path]), (directory) => directory.createDirectoryAt(path)),

    removeDirectoryAt: (call, [self, path]: [HostDescriptor, string]) =>
      attempt(call, mutableAt(self, [path]), (directory) => directory.removeDirectoryAt(path)),

    unlinkFileAt: (call, [self, path]: [HostDescriptor, string]) =>
      attempt(call, mutableAt(self, [path]), (directory) => directory.unlinkFileAt(path)),

    renameAt: (call, [self, from, target, to]: [HostDescriptor, string, HostDescriptor, string]) =>
      attempt(call, mutableAt(self, [from, to], target), (directory) =>
        directory.renameAt(from, target.directory!, to),
      ),

    async linkAt(
      _call,
      [self, , from, target, to]: [HostDescriptor, unknown, string, HostDescriptor, string],
    ) {
      const checked = mutableAt(self, [from, to], target);
      return isResult(checked) ? checked : err("unsupported");
    },

    async symlinkAt(_call, [self, , to]: [HostDescriptor, string, string]) {
      const checked = mutableAt(self, [to]);
      return isResult(checked) ? checked : err("unsupported");
    },

    async isSameObject(call, [self, other]: [HostDescriptor, HostDescriptor]) {
      try {
        return await provider(call, () => self.provider.isSameObject(other.provider));
      } catch (error) {
        if (call.signal?.aborted || !(error instanceof FilesystemError)) throw error;
        return false;
      }
    },

    metadataHash: (call, [self]: [HostDescriptor]) =>
      attempt(call, self.provider, (p) => p.stat().then(metadataHash)),

    metadataHashAt: (call, [self, , path]: [HostDescriptor, unknown, string]) =>
      attempt(call, directoryAt(self, path), (directory) =>
        directory.statAt(path).then(metadataHash),
      ),
  };
  // Resource methods are keyed by their WIT interface like free functions.
  return Object.fromEntries(
    Object.entries(methods).map(([name, method]) => [
      name.includes("#") ? name : `${TYPES}#${name}`,
      method,
    ]),
  );
}
