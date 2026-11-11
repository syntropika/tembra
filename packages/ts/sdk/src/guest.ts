import { utf8Decode, utf8Encode } from "./guest/utf8.ts";

/** The guest observes cancellation without receiving an EventTarget or host context identity. */
export interface Signal {
  readonly aborted: boolean;
  throwIfAborted(): void;
}

/**
 * The context of one admitted callback. It is valid only inside the callback that received it:
 * a context retained into a later callback, or used after its callback ended, is refused before
 * any host call.
 */
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

/**
 * A host reference to a WIT Orb dependency. Each method is the dependency's own typed WIT import:
 * a synchronous WIT function returns its value, an `async` one returns a Promise. A trailing
 * context argument is optional and only checked; the host attributes the call to the running
 * callback.
 */
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
    ? (...args: [...Arguments, context?: Context]) => Result
    : never;
};

/** The provider's run of one of its own hooks, through the hook's typed WIT function. */
export interface Hook<Input, Output> {
  run(context: Context, input: Input): Output;
}

export interface Declaration {
  readonly key: readonly string[];
  readonly principal: string;
  readonly orb: unknown;
  state<T>(
    initial: T,
    options?: { version?: number; validate?(value: Readonly<T>): boolean | void },
  ): State<T>;
  get<Interface>(binding: string, key?: string | readonly string[]): WitReference<Interface>;
  /**
   * Bind a hook this Orb provides. `Output` is the hook function's WIT result: a value for a
   * synchronous WIT function, a Promise for an `async` one.
   */
  hook<Input = unknown, Output = Input>(name: string): Hook<Input, Output>;
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

/** A synchronous WIT result: a returned Promise is refused, including for `void` functions. */
type Synchronous<Result> = [Result] extends [void] ? undefined : Result;

/**
 * Typed implementations of one exported WIT interface. A synchronous WIT function requires a
 * synchronous implementation; declare the WIT function `async` to implement it asynchronously.
 */
export type WitMethods<Interface, Environment> = {
  [
    Method in keyof Interface as Interface[Method] extends (...args: never[]) => unknown
      ? Method
      : never
  ]: Interface[Method] extends (...args: infer Arguments) => infer Result
    ? // Not distributive: an `option<T>` result (`T | undefined`) is one function type.
      [Result] extends [Promise<infer Value>]
      ? (
          environment: Environment,
          context: Context,
          ...args: Arguments
        ) => Value | PromiseLike<Value>
      : (environment: Environment, context: Context, ...args: Arguments) => Synchronous<Result>
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
 * actual type, for example `ReturnType<typeof create>`. It implements the hook interface's WIT
 * function, so it is synchronous unless that function is `async`.
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

/**
 * The readable end of a WIT `future<T>` as the component runtime lifts it. It is deliberately not
 * thenable: `read()` returns the value once.
 */
export interface WitFuture<T> {
  read(): Promise<T>;
  drop(): void;
}

/**
 * Iterate a WIT `stream<u8>` as `Uint8Array` chunks and release it when iteration ends.
 *
 * The component runtime already lifts `stream<u8>` as bounded `Uint8Array` chunks; this keeps the
 * early-exit release explicit for readers that stop before the end.
 */
export async function* readChunks(
  stream: AsyncIterable<Uint8Array>,
): AsyncGenerator<Uint8Array, void, undefined> {
  try {
    for await (const chunk of stream) yield chunk;
  } finally {
    dropStream(stream);
  }
}

/** Default bound of `readBytes`/`readText`. */
const READ_LIMIT = 16 << 20;

/**
 * Pass `Uint8Array` chunks as a WIT `stream<u8>` argument or result. The component runtime
 * writes each chunk with backpressure and reads the source lazily: do not mutate a chunk until
 * the next is requested. The source's `return()` runs if the consumer stops early.
 */
export function writeChunks(
  chunks: Iterable<Uint8Array> | AsyncIterable<Uint8Array>,
): AsyncIterable<Uint8Array> {
  if (Symbol.asyncIterator in chunks) return chunks;
  return (async function* () {
    yield* chunks;
  })();
}

/** The `result<_, error-code>` payload of a filesystem stream future. */
export type WasiResult = { tag: "ok"; val?: unknown } | { tag: "err"; val: unknown };

/** The part of a standard `wasi:filesystem` descriptor the read helpers use. */
export interface ReadableFile {
  readViaStream(offset: bigint): readonly [AsyncIterable<Uint8Array>, WitFuture<WasiResult>];
}

/** The part of a standard `wasi:filesystem` descriptor the write helpers use. */
export interface WritableFile {
  writeViaStream(data: AsyncIterable<Uint8Array>, offset: bigint): WitFuture<WasiResult>;
  appendViaStream(data: AsyncIterable<Uint8Array>): WitFuture<WasiResult>;
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
  (stream as { drop?: () => void } | null)?.drop?.();
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
    check(await done.read());
    if (chunks.length === 1) return chunks[0]!;
    const bytes = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return bytes;
  } finally {
    // An early failure leaves the completion future unread; drop it too.
    if (!consumed) done.drop();
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
  const done = options.append
    ? file.appendViaStream(data)
    : file.writeViaStream(data, options.offset ?? 0n);
  check(await done.read());
}

/** Encode `text` as UTF-8 and `writeBytes` it. */
export function writeText(
  file: WritableFile,
  text: string,
  options: { offset?: bigint; append?: boolean } = {},
): Promise<void> {
  return writeBytes(file, utf8Encode(text), options);
}
