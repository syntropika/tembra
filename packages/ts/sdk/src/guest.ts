/** The guest observes cancellation without receiving an EventTarget or host context identity. */
export interface Signal {
  readonly aborted: boolean;
  throwIfAborted(): void;
}

export interface Context {
  readonly principal: string;
  check(): void;
  cancel(): void;
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
    filesystem?: {
      readText(path: string): Promise<string>;
      writeText(path: string, value: string): Promise<void>;
    };
    transport?: {
      /** Open one byte stream to an exactly granted literal IP address and port. */
      connect(ip: string, port: number): Promise<TransportStream>;
    };
  };
}

/**
 * One incremental byte stream issued to the active callback. It closes when that callback
 * completes. Reads return at most 16 KiB, or `null` at end of stream; writes accept at most
 * 16 KiB per call.
 */
export interface TransportStream {
  read(): Promise<Uint8Array | null>;
  write(bytes: Uint8Array): Promise<void>;
  close(): Promise<void>;
}
