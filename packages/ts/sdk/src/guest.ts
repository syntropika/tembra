import { utf8Decode, utf8Encode } from "./interpreter/standard-utf8.ts";

/** The guest observes cancellation without receiving an EventTarget or host context identity. */
export interface Signal {
  readonly aborted: boolean;
  throwIfAborted(): void;
}

export interface Context {
  readonly principal: string;
  check(): void;
  cancel(): void;
  /**
   * Acquire a selected WIT Orb import by key for this context's root and principal. The host
   * applies the installed requirement and waits for readiness; the claim ends with the root, and
   * the reference is refused in every other root.
   */
  get<Interface>(
    binding: string,
    key: string | readonly string[],
  ): Promise<WitReference<Interface>>;
}

export interface Readable<T> {
  readonly value: Readonly<T>;
  select<R>(compute: (value: Readonly<T>) => R): Readable<R>;
}

export interface State<T> extends Readable<T> {
  update(context: Context, change: (current: Readonly<T>) => T): Readonly<T>;
}

export interface Resource<T> {
  readonly value: T;
  readonly version: number;
}

export type WitReference<Interface> = {
  readonly ready: Promise<void>;
  readonly status: string;
  readonly state: Readable<unknown>;
  release(): void;
} & {
  [
    Method in keyof Interface as Interface[Method] extends (...args: never[]) => unknown
      ? Method
      : never
  ]: Interface[Method] extends (...args: infer Arguments) => infer Result
    ? (...args: [...Arguments, context?: Context]) => Promise<Awaited<Result>>
    : never;
};

export interface Declaration {
  readonly key: readonly string[];
  readonly principal: string;
  readonly orb: unknown;
  state<T>(
    initial: T,
    options?: { version?: number; validate?(value: Readonly<T>): boolean | void },
  ): State<T>;
  get<Interface>(binding: string, key?: string | readonly string[]): WitReference<Interface>;
  hook<Input = unknown, Output = Input>(
    name: string,
  ): {
    run(context: Context, input: Input): Promise<Output>;
  };
  resource<T>(
    name: string,
    options: {
      version?: number;
      deps?: readonly Resource<unknown>[];
      open(signal: Signal): T | PromiseLike<T>;
      close?(value: T, context: Context): unknown;
    },
  ): Resource<T>;
}

export type WitMethods<Interface, Environment> = {
  [
    Method in keyof Interface as Interface[Method] extends (...args: never[]) => unknown
      ? Method
      : never
  ]: Interface[Method] extends (...args: infer Arguments) => infer Result
    ? (
        environment: Environment,
        context: Context,
        ...args: Arguments
      ) => Awaited<Result> | PromiseLike<Awaited<Result>>
    : never;
};

interface Service {
  create?(self: Declaration): unknown;
  methods: Record<string, (...args: never[]) => unknown>;
  lifetime?: "referenced" | "host";
  linger?: number;
}

export interface HookComposition {
  readonly composition: "series" | "waterfall" | "bail";
}

/** Observe each contribution in the kernel's plan order. */
export function series(): HookComposition {
  return { composition: "series" };
}
/** Pass each contribution's result to the next participant. */
export function waterfall(): HookComposition {
  return { composition: "waterfall" };
}
/** Return the first defined contribution result. */
export function bail(): HookComposition {
  return { composition: "bail" };
}

/**
 * A source contribution receives the Orb root environment returned by `orb.create`, then the
 * issued context and the hook input, like a service method. Annotate the environment with its
 * actual type, for example `ReturnType<typeof create>`.
 */
export type Contribution =
  | ((environment: never, context: Context, input: never) => unknown)
  | {
      handle(environment: never, context: Context, input: never): unknown;
      priority?: number;
    };

/** Retain literal identity and callbacks; the outer host validates them against WIT before admission. */
export function orb<
  const Definition extends {
    name: string;
    apiVersion: number;
    create?(self: Declaration): unknown;
    services?: Record<string, Service>;
    hooks?: Readonly<Record<string, HookComposition>>;
    contributes?: Readonly<Record<string, Contribution>>;
  },
>(definition: Definition): Readonly<Definition> {
  return Object.freeze(definition);
}

declare global {
  interface Console {
    log(...values: unknown[]): void;
    debug(...values: unknown[]): void;
    info(...values: unknown[]): void;
    warn(...values: unknown[]): void;
    error(...values: unknown[]): void;
  }
  var console: Console;
  const tembra: {
    wait(milliseconds: number): Promise<void>;
  };
}

/** Chunk reader the interpreter attaches to every standard `stream<u8>` it issues. */
const READ_CHUNK = Symbol.for("tembra.wasi.stream.read-chunk");
/** Fallback batch size for byte streams that expose only per-byte iteration. */
const BATCH_BYTES = 64 << 10;

/**
 * Iterate a standard WIT `stream<u8>` as `Uint8Array` chunks.
 *
 * jco declares `stream<u8>` as `AsyncIterable<number>`, and the interpreter's streams honor that
 * contract, but awaiting one element per byte spends the callback's instruction budget on every
 * byte. Streams issued by the interpreter already transfer bounded raw chunks (at most 64 KiB);
 * this returns those chunks unchanged. Any other `AsyncIterable<number>` is batched.
 */
export async function* readChunks(
  stream: AsyncIterable<number>,
): AsyncGenerator<Uint8Array, void, undefined> {
  const read = (stream as { [READ_CHUNK]?: () => Promise<Uint8Array | undefined> })[READ_CHUNK];
  if (typeof read === "function") {
    try {
      for (let chunk = await read.call(stream); chunk; chunk = await read.call(stream)) yield chunk;
    } finally {
      dropStream(stream);
    }
    return;
  }
  let batch = new Uint8Array(BATCH_BYTES);
  let length = 0;
  for await (const byte of stream) {
    batch[length++] = byte;
    if (length === BATCH_BYTES) {
      yield batch;
      batch = new Uint8Array(BATCH_BYTES);
      length = 0;
    }
  }
  if (length) yield batch.slice(0, length);
}

/** Chunk source the interpreter recognizes on a `stream<u8>` argument. */
const WRITE_CHUNKS = Symbol.for("tembra.wasi.stream.write-chunks");
/** Default bound of `readBytes`/`readText`. */
const READ_LIMIT = 16 << 20;

/**
 * Pass `Uint8Array` chunks as a standard WIT `stream<u8>` argument.
 *
 * The result is the `AsyncIterable<number>` jco declares, so it type-checks wherever a
 * `stream<u8>` is expected. The interpreter recognizes it and transfers each chunk in raw
 * pieces of at most 64 KiB with backpressure instead of iterating bytes; any other consumer
 * sees the bytes one by one. Chunks are read lazily; do not mutate one until the next is
 * requested. The source's `return()` runs if the consumer stops early.
 */
export function writeChunks(
  chunks: Iterable<Uint8Array> | AsyncIterable<Uint8Array>,
): AsyncIterable<number> {
  const iterator = () =>
    Symbol.asyncIterator in chunks
      ? (chunks as AsyncIterable<Uint8Array>)[Symbol.asyncIterator]()
      : (chunks as Iterable<Uint8Array>)[Symbol.iterator]();
  return {
    [WRITE_CHUNKS]: iterator,
    async *[Symbol.asyncIterator]() {
      for await (const chunk of chunks) yield* chunk;
    },
  } as AsyncIterable<number>;
}

/** jco's `result<_, error-code>` payload of a filesystem stream future. */
export type WasiResult = { tag: "ok"; val?: unknown } | { tag: "err"; val: unknown };

/** The part of a standard `wasi:filesystem` descriptor the read helpers use. */
export interface ReadableFile {
  readViaStream(offset: bigint): readonly [AsyncIterable<number>, PromiseLike<WasiResult>];
}

/** The part of a standard `wasi:filesystem` descriptor the write helpers use. */
export interface WritableFile {
  writeViaStream(data: AsyncIterable<number>, offset: bigint): PromiseLike<WasiResult>;
  appendViaStream(data: AsyncIterable<number>): PromiseLike<WasiResult>;
}

/** A filesystem helper failed with a WASI `error-code`; `code` is its tag. */
export class FilesystemCallError extends Error {
  readonly code: string;
  constructor(readonly payload: unknown) {
    const code = String((payload as { tag?: unknown } | null)?.tag ?? "other");
    super(`filesystem call failed: ${code}`);
    this.code = code;
  }
}

/** Throw the error case of a stream future's result. */
function check(result: WasiResult): void {
  if (result?.tag === "err") throw new FilesystemCallError(result.val);
}

/** Drop a stream or unread future handle now instead of waiting for collection. */
function dropStream(stream: unknown): void {
  const dispose = (Symbol as { dispose?: symbol }).dispose ?? Symbol.for("Symbol.dispose");
  (stream as Record<symbol, (() => void) | undefined>)[dispose]?.();
}

/**
 * Read a file from `offset` (default `0n`) to its end through `read-via-stream`. More than
 * `maxBytes` (default 16 MiB) is refused with a `RangeError` instead of growing without bound.
 */
export async function readBytes(
  file: ReadableFile,
  options: { offset?: bigint; maxBytes?: number } = {},
): Promise<Uint8Array> {
  const limit = options.maxBytes ?? READ_LIMIT;
  const [stream, done] = file.readViaStream(options.offset ?? 0n);
  let consumed = false;
  try {
    const chunks: Uint8Array[] = [];
    let total = 0;
    for await (const chunk of readChunks(stream)) {
      total += chunk.byteLength;
      if (total > limit) throw new RangeError(`file exceeds the ${limit} byte read bound`);
      chunks.push(chunk);
    }
    consumed = true;
    check(await done);
    if (chunks.length === 1) return chunks[0]!;
    const bytes = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return bytes;
  } finally {
    dropStream(stream);
    // An early failure leaves the completion future unread; drop it too.
    if (!consumed) dropStream(done);
  }
}

/** `readBytes`, then strict UTF-8 decoding; invalid UTF-8 is a `RangeError`. */
export async function readText(
  file: ReadableFile,
  options: { offset?: bigint; maxBytes?: number } = {},
): Promise<string> {
  return utf8Decode(await readBytes(file, options));
}

/**
 * Write `bytes` at `offset` (default `0n`), or at the end of file with `append`, through
 * `write-via-stream`/`append-via-stream`. It never truncates: open the file with the
 * `truncate` flag to replace its contents.
 */
export async function writeBytes(
  file: WritableFile,
  bytes: Uint8Array,
  options: { offset?: bigint; append?: boolean } = {},
): Promise<void> {
  const data = writeChunks([bytes]);
  check(
    await (options.append
      ? file.appendViaStream(data)
      : file.writeViaStream(data, options.offset ?? 0n)),
  );
}

/** Encode `text` as UTF-8 and `writeBytes` it. */
export function writeText(
  file: WritableFile,
  text: string,
  options: { offset?: bigint; append?: boolean } = {},
): Promise<void> {
  return writeBytes(file, utf8Encode(text), options);
}
