/** Every standard `wasi:filesystem` `error-code` case except `other`; other failures surface as `io`. */
export type FilesystemErrorCode =
  | "access"
  | "already"
  | "bad-descriptor"
  | "busy"
  | "deadlock"
  | "quota"
  | "exist"
  | "file-too-large"
  | "illegal-byte-sequence"
  | "in-progress"
  | "interrupted"
  | "invalid"
  | "io"
  | "is-directory"
  | "loop"
  | "too-many-links"
  | "message-size"
  | "name-too-long"
  | "no-device"
  | "no-entry"
  | "no-lock"
  | "insufficient-memory"
  | "insufficient-space"
  | "not-directory"
  | "not-empty"
  | "not-recoverable"
  | "unsupported"
  | "no-tty"
  | "no-such-device"
  | "overflow"
  | "not-permitted"
  | "pipe"
  | "read-only"
  | "invalid-seek"
  | "text-file-busy"
  | "cross-device";

/** A provider failure carrying the WASI `error-code` that standard filesystem calls return. */
export class FilesystemError extends Error {
  constructor(
    readonly code: FilesystemErrorCode,
    message: string = code,
  ) {
    super(message);
  }
}

export type EntryType = "file" | "directory";

/** Metadata of one entry; `modified` is milliseconds since the Unix epoch when known. */
export interface FileStat {
  readonly type: EntryType;
  readonly size: bigint;
  readonly modified?: number;
}

export interface DirectoryEntry {
  readonly name: string;
  readonly type: EntryType;
}

/** `open-at` flags plus the requested write right (`write` or `mutate-directory`). */
export interface OpenOptions {
  readonly create?: boolean;
  readonly exclusive?: boolean;
  readonly truncate?: boolean;
  readonly directory?: boolean;
  readonly write?: boolean;
}

interface DescriptorBase {
  readonly type: EntryType;
  /** Write (file) or mutate-directory (directory) right of this descriptor. */
  readonly writable: boolean;
  stat(): Promise<FileStat>;
  isSameObject(other: Descriptor): Promise<boolean>;
  sync(): Promise<void>;
  /** Idempotent; later calls fail with `bad-descriptor` while in-flight calls settle normally. */
  close(): void;
}

/** An opened regular file; it follows its entry across rename where the platform allows. */
export interface FileDescriptor extends DescriptorBase {
  readonly type: "file";
  /** Read at most `length` bytes at `offset`; an empty result is end of file. */
  read(offset: bigint, length: number): Promise<Uint8Array>;
  /** Write every byte at `offset`, zero-filling any gap; `data` must stay unchanged until settled. */
  write(offset: bigint, data: Uint8Array): Promise<void>;
  /** Write every byte at the current end of file. */
  append(data: Uint8Array): Promise<void>;
  /** Truncate or zero-extend. */
  setSize(size: bigint): Promise<void>;
  /**
   * Optional; built-in providers implement it. Write sequential chunks from `offset`, or from the
   * end of file when it is absent (`append-via-stream`). The next chunk is requested only after
   * the previous one is written; early exits call the iterator's `return()`. Accepted chunks are
   * kept when the producer fails or the descriptor closes. OPFS commits once when the iterable
   * ends, and readers observe the previous committed content until then.
   */
  writeStream?(chunks: AsyncIterable<Uint8Array>, offset?: bigint): Promise<void>;
}

/** An opened directory; relative paths resolve from it and never above it. */
export interface DirectoryDescriptor extends DescriptorBase {
  readonly type: "directory";
  openAt(path: string, options?: OpenOptions): Promise<Descriptor>;
  statAt(path: string): Promise<FileStat>;
  /** Lazy entries in platform order, without `.` and `..`. */
  readDirectory(): AsyncIterableIterator<DirectoryEntry>;
  createDirectoryAt(path: string): Promise<void>;
  removeDirectoryAt(path: string): Promise<void>;
  unlinkFileAt(path: string): Promise<void>;
  /** POSIX `renameat`; `newDirectory` must be an open descriptor of the same storage. */
  renameAt(oldPath: string, newDirectory: DirectoryDescriptor, newPath: string): Promise<void>;
}

export type Descriptor = FileDescriptor | DirectoryDescriptor;

/** One grant's permission and lifetime scope over the provider's shared storage. */
export interface MountedFilesystem {
  readonly writable: boolean;
  /** A new owned descriptor for the mount root. */
  openRoot(): DirectoryDescriptor;
  /** Close every descriptor of this scope now; settles after its in-flight calls settle. */
  dispose(): Promise<void>;
}

export interface FilesystemProvider {
  /** Open independent permission and lifetime scopes over explicitly shared storage. */
  open(mount: string, writable: boolean): MountedFilesystem;
}

/** Throw the standard error for `code`; shared by the private provider modules. */
export function fail(code: FilesystemErrorCode, message?: string): never {
  throw new FilesystemError(code, message);
}
