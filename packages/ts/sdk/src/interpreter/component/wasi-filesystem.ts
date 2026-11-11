/**
 * `wasi:filesystem@0.3.0` for trusted JCO bindings over the granted provider mounts
 * (`filesystem.ts`: MemoryFS, OPFS or embedder providers).
 *
 * Tembra checks descriptor kind, rights and lexical confinement before any provider call, then
 * providers enforce their own scope as well. Error codes follow upstream Wasmtime: a wrong
 * descriptor kind or a missing file right is `bad-descriptor` (`not-directory` for directory
 * methods), and a directory mutation without `mutate-directory` is `not-permitted`. Browser
 * providers have no settable timestamps, hard links or symbolic links: those methods report
 * `unsupported` after their authority checks.
 */
import {
  FilesystemError,
  type Descriptor as ProviderDescriptor,
  type DirectoryDescriptor,
  type FileDescriptor,
  type FileStat,
  type MountedFilesystem,
} from "../filesystem.ts";
import {
  err,
  guestBytes,
  hostBytes,
  hostItems,
  ok,
  type WasiHost,
  type WasiInterfaces,
  type WasiResult,
} from "./wasi-host.ts";

/** A mount granted to this revision. */
export interface StandardMount {
  readonly path: string;
  readonly writable: boolean;
  readonly filesystem: MountedFilesystem;
}

interface DescriptorFlags {
  read: boolean;
  write: boolean;
  fileIntegritySync: boolean;
  dataIntegritySync: boolean;
  requestedWriteSync: boolean;
  mutateDirectory: boolean;
}
type Timestamp = { tag: "no-change" | "now" | "timestamp"; val?: unknown };
const U64 = (1n << 64n) - 1n;
const failure = (code: string): WasiResult => err({ tag: code });

/** Map a provider failure to its WASI error code; anything else is `io`. */
function code(error: unknown): string {
  return error instanceof FilesystemError ? error.code : "io";
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

function instant(milliseconds: number | undefined) {
  if (milliseconds === undefined || !Number.isFinite(milliseconds)) return undefined;
  const whole = Math.floor(milliseconds);
  const seconds = Math.floor(whole / 1000);
  return {
    seconds: BigInt(seconds),
    nanoseconds: (whole - seconds * 1000) * 1_000_000,
  };
}

const descriptorType = (type: "file" | "directory") => ({
  tag: type === "file" ? "regular-file" : "directory",
});

const descriptorStat = (stat: FileStat) => ({
  type: descriptorType(stat.type),
  linkCount: 1n,
  size: stat.size,
  dataAccessTimestamp: undefined,
  dataModificationTimestamp: instant(stat.modified),
  statusChangeTimestamp: undefined,
});

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

/** The `wasi:filesystem/types` and `preopens` import objects of one instance. */
export function filesystemImports(
  host: WasiHost,
  mounts: () => readonly StandardMount[],
): WasiInterfaces<"types" | "preopens"> {
  /** A provider descriptor behind one guest handle; streams retain it independently. */
  class Descriptor {
    #users = 1;
    #closed = false;
    constructor(
      readonly provider: ProviderDescriptor,
      readonly flags: Readonly<DescriptorFlags>,
    ) {}

    get file(): FileDescriptor | undefined {
      return this.provider.type === "file" ? this.provider : undefined;
    }
    get directory(): DirectoryDescriptor | undefined {
      return this.provider.type === "directory" ? this.provider : undefined;
    }
    get mutable(): boolean {
      return this.provider.type === "directory" && this.flags.mutateDirectory;
    }
    retain(): void {
      this.#users++;
    }
    release(): void {
      if (--this.#users === 0) host.cleanup(() => this.provider.close() as unknown);
    }
    /** The guest dropped its handle; streams may still hold the provider descriptor. */
    [Symbol.dispose](): void {
      if (this.#closed) return;
      this.#closed = true;
      this.release();
    }

    readViaStream(offset: bigint) {
      const file = this.file;
      if (!file || !this.flags.read) return failedOutput("bad-descriptor");
      let position = offset;
      return output(this, async (limit) => {
        const bytes = await host.physical(() => file.read(position, limit));
        position += BigInt(bytes.byteLength);
        return bytes;
      });
    }
    writeViaStream(data: unknown, offset: bigint) {
      return consume(this, data, offset);
    }
    appendViaStream(data: unknown) {
      return consume(this, data, undefined);
    }
    readDirectory() {
      const directory = this.directory;
      if (!directory)
        return [
          hostItems(
            async () => undefined,
            () => {},
            host.retired,
          ),
          Promise.resolve(failure("not-directory")),
        ];
      let entries: ReturnType<DirectoryDescriptor["readDirectory"]>;
      try {
        entries = directory.readDirectory();
      } catch (error) {
        return [
          hostItems(
            async () => undefined,
            () => {},
            host.retired,
          ),
          Promise.resolve(failure(code(error))),
        ];
      }
      let settle!: (value: WasiResult) => void;
      const result = new Promise<WasiResult>((resolve) => (settle = resolve));
      let done = false;
      this.retain();
      const stream = hostItems(
        async () => {
          const next = await host.physical(() => entries.next());
          if (next.done) {
            done = true;
            return undefined;
          }
          return {
            type: descriptorType(next.value.type),
            name: next.value.name,
          };
        },
        (end, error) => {
          if (!done) host.cleanup(() => entries.return?.());
          this.release();
          settle(end === "failed" ? failure(code(error)) : ok());
        },
        host.retired,
      );
      return [stream, result];
    }
    async advise() {
      return this.file ? ok() : failure("bad-descriptor");
    }
    sync() {
      return attempt(this.provider, (p) => p.sync());
    }
    syncData() {
      return attempt(this.provider, (p) => p.sync());
    }
    async getFlags() {
      return ok({ ...this.flags });
    }
    async getType() {
      return ok(descriptorType(this.provider.type));
    }
    setSize(size: bigint) {
      return attempt(
        this.file && this.flags.write ? this.file : failure("bad-descriptor"),
        (file) => file.setSize(size),
      );
    }
    async setTimes(accessed: Timestamp, modified: Timestamp) {
      if (this.file ? !this.flags.write : !this.mutable)
        return failure(this.file ? "bad-descriptor" : "not-permitted");
      return accessed.tag === "no-change" && modified.tag === "no-change"
        ? ok()
        : failure("unsupported");
    }
    async setTimesAt(_flags: unknown, path: string, accessed: Timestamp, modified: Timestamp) {
      const checked = mutableAt(this, [path]);
      if (isResult(checked)) return checked;
      if (accessed.tag !== "no-change" || modified.tag !== "no-change")
        return failure("unsupported");
      return attempt(checked, (directory) => directory.statAt(path).then(() => undefined));
    }
    stat() {
      return attempt(this.provider, (p) => p.stat().then(descriptorStat));
    }
    statAt(_flags: unknown, path: string) {
      return attempt(directoryAt(this, path), (directory) =>
        directory.statAt(path).then(descriptorStat),
      );
    }
    async openAt(
      _flags: unknown,
      path: string,
      open: {
        create?: boolean;
        directory?: boolean;
        exclusive?: boolean;
        truncate?: boolean;
      },
      flags: Partial<DescriptorFlags>,
    ) {
      const write = Boolean(flags.write || flags.mutateDirectory);
      const directory = this.directory;
      if (!directory) return failure("not-directory");
      // Write authority comes only from a mutable directory, checked before the provider.
      if ((open.create || open.truncate || write) && !this.mutable) return failure("not-permitted");
      const refused = refusal(path);
      if (refused) return failure(refused);
      let opened: ProviderDescriptor;
      try {
        opened = await host.physical(() =>
          directory.openAt(path, {
            create: Boolean(open.create),
            exclusive: Boolean(open.exclusive),
            truncate: Boolean(open.truncate),
            directory: Boolean(open.directory),
            write,
          }),
        );
      } catch (error) {
        return failure(code(error));
      }
      const granted = opened.writable && write;
      return ok(
        new Descriptor(opened, {
          read: opened.type === "directory" || Boolean(flags.read),
          write: opened.type === "file" && granted,
          fileIntegritySync: Boolean(flags.fileIntegritySync),
          dataIntegritySync: Boolean(flags.dataIntegritySync),
          requestedWriteSync: Boolean(flags.requestedWriteSync),
          mutateDirectory: opened.type === "directory" && granted,
        }),
      );
    }
    async readlinkAt(path: string) {
      // There are no symbolic links: an existing entry is not one (`invalid`), as in POSIX.
      const result = await attempt(directoryAt(this, path), (directory) => directory.statAt(path));
      return result.tag === "ok" ? failure("invalid") : result;
    }
    createDirectoryAt(path: string) {
      return attempt(mutableAt(this, [path]), (directory) => directory.createDirectoryAt(path));
    }
    removeDirectoryAt(path: string) {
      return attempt(mutableAt(this, [path]), (directory) => directory.removeDirectoryAt(path));
    }
    unlinkFileAt(path: string) {
      return attempt(mutableAt(this, [path]), (directory) => directory.unlinkFileAt(path));
    }
    renameAt(from: string, target: Descriptor, to: string) {
      own(target);
      return attempt(mutableAt(this, [from, to], target), (directory) =>
        directory.renameAt(from, target.directory!, to),
      );
    }
    async linkAt(_flags: unknown, from: string, target: Descriptor, to: string) {
      own(target);
      const checked = mutableAt(this, [from, to], target);
      return isResult(checked) ? checked : failure("unsupported");
    }
    async symlinkAt(_from: string, to: string) {
      const checked = mutableAt(this, [to]);
      return isResult(checked) ? checked : failure("unsupported");
    }
    async isSameObject(other: Descriptor) {
      own(other);
      try {
        return await host.physical(() => this.provider.isSameObject(other.provider));
      } catch (error) {
        if (!(error instanceof FilesystemError)) throw error;
        return false;
      }
    }
    metadataHash() {
      return attempt(this.provider, (p) => p.stat().then(metadataHash));
    }
    metadataHashAt(_flags: unknown, path: string) {
      return attempt(directoryAt(this, path), (directory) =>
        directory.statAt(path).then(metadataHash),
      );
    }
  }

  /** A borrowed descriptor must be one this instance issued. */
  const own = (value: unknown): void => {
    if (!(value instanceof Descriptor)) throw new TypeError("Foreign filesystem descriptor");
  };
  const isResult = (value: unknown): value is WasiResult =>
    typeof value === "object" && value !== null && "tag" in value && !(value instanceof Descriptor);

  function directoryAt(self: Descriptor, ...paths: string[]): DirectoryDescriptor | WasiResult {
    const directory = self.directory;
    if (!directory) return failure("not-directory");
    for (const path of paths) {
      const refused = refusal(path);
      if (refused) return failure(refused);
    }
    return directory;
  }
  function mutableAt(
    self: Descriptor,
    paths: string[],
    other?: Descriptor,
  ): DirectoryDescriptor | WasiResult {
    if (!self.directory || (other && !other.directory)) return failure("not-directory");
    if (!self.mutable || (other && !other.mutable)) return failure("not-permitted");
    return directoryAt(self, ...paths);
  }
  async function attempt<T>(
    checked: T | WasiResult,
    operation: (target: T) => Promise<unknown>,
  ): Promise<WasiResult> {
    if (isResult(checked)) return checked;
    try {
      return ok(await host.physical(() => operation(checked)));
    } catch (error) {
      if (!(error instanceof FilesystemError) && host.retired.aborted) throw error;
      return failure(code(error));
    }
  }
  /** A stream method's failure is reported through its future, as upstream does. */
  function failedOutput(reason: string) {
    return [
      hostBytes(
        async () => undefined,
        () => {},
        host.retired,
      ),
      Promise.resolve(failure(reason)),
    ];
  }
  function output(self: Descriptor, next: (limit: number) => Promise<Uint8Array>) {
    let settle!: (value: WasiResult) => void;
    const result = new Promise<WasiResult>((resolve) => (settle = resolve));
    // The stream keeps its descriptor's provider object open after the handle is dropped.
    self.retain();
    const stream = hostBytes(
      next,
      (end, error) => {
        self.release();
        settle(end === "failed" ? failure(code(error)) : ok());
      },
      host.retired,
    );
    return [stream, result];
  }
  /**
   * Consume a guest stream into a file; the future reports the outcome. A provider with
   * `writeStream` receives the whole stream in one call (one OPFS writer and commit); otherwise
   * each chunk is a sequential `write`/`append`. The descriptor stays open and the work stays
   * physically owned until the provider settles.
   */
  function consume(
    self: Descriptor,
    data: unknown,
    offset: bigint | undefined,
  ): Promise<WasiResult> {
    const input = guestBytes(data, host.retired);
    const file = self.file;
    if (!file || !self.flags.write) {
      void input.return!();
      return Promise.resolve(failure("bad-descriptor"));
    }
    self.retain();
    const outcome = (async (): Promise<WasiResult> => {
      try {
        if (file.writeStream) {
          await host.physical(() => file.writeStream!(input, offset));
          return ok();
        }
        let position = offset;
        for await (const bytes of input) {
          if (position === undefined) await host.physical(() => file.append(bytes));
          else {
            const at = position;
            await host.physical(() => file.write(at, bytes));
            position += BigInt(bytes.byteLength);
          }
        }
        return ok();
      } catch (error) {
        return failure(code(error));
      } finally {
        void input.return!();
        self.release();
      }
    })();
    host.cleanup(() => outcome);
    return outcome;
  }

  const root = (mount: StandardMount) =>
    new Descriptor(mount.filesystem.openRoot(), {
      read: true,
      write: false,
      fileIntegritySync: false,
      dataIntegritySync: false,
      requestedWriteSync: false,
      mutateDirectory: mount.writable,
    });
  return {
    types: {
      Descriptor,
      filesystemErrorCode: () => undefined,
    },
    preopens: {
      // Like upstream, every call issues fresh owned descriptors for the preopened mounts.
      getDirectories: () => mounts().map((mount) => [root(mount), mount.path]),
    },
  };
}
