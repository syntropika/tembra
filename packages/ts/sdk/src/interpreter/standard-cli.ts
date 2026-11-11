/** Standard CLI over host-owned byte providers, with no inherited process environment. */
import type { CliOptions } from "../authoring/types.ts";
import { combinedSignal } from "../runtime/signals.ts";
import { GuestTrap } from "./core.ts";
import {
  HostFuture,
  type Implementation,
  type InputStream,
  OutputStream,
  type StandardServices,
} from "./standard-streams.ts";

/** The promise owns the actual read until it returns, including after signal cancellation. */
export interface StdioInput {
  read(maxBytes: number, signal: AbortSignal): Promise<Uint8Array | undefined>;
  close?(): void | Promise<void>;
}

/** The promise settles only after the physical write has returned. Calls are serialized. */
export interface StdioOutput {
  write(bytes: Uint8Array, signal: AbortSignal): Promise<void>;
  close?(): void | Promise<void>;
}

/** A fresh set per revision; an output provider can be shared by stdout and stderr. */
export interface StdioStreams {
  readonly stdin?: StdioInput;
  readonly stdout?: StdioOutput;
  readonly stderr?: StdioOutput;
}

/** The embedding chooses adapters; Tembra owns their operation promises and final close. */
export type StdioProvider = (orb: string, configuration: Readonly<CliOptions>) => StdioStreams;

const ok = () => ({ tag: "ok" });
const io = () => ({ tag: "err", val: "io" });

/** Reuse the revision's existing resource table and physical-work accounting for CLI streams. */
export class StandardCli {
  readonly #input: StdioInput | undefined;
  readonly #providers: Set<StdioInput | StdioOutput>;
  readonly #writes = new Map<StdioOutput, Promise<void>>();
  readonly #stopped = new AbortController();
  #reading: Promise<void> = Promise.resolve();
  #closed = false;
  readonly methods: Record<string, Implementation>;

  constructor(configuration: Readonly<CliOptions> = {}, streams: StdioStreams = {}) {
    this.#providers = new Set();
    for (const provider of [streams.stdin, streams.stdout, streams.stderr])
      if (provider) this.#providers.add(provider);
    const input = configuration.stdin;
    if (typeof input === "object" && "file" in input && !streams.stdin)
      throw new Error("Browser stdin files require an injected stdio provider");
    if (input === "empty") this.#input = undefined;
    else if (typeof input === "object" && "bytes" in input) {
      const bytes = input.bytes.slice();
      let offset = 0;
      this.#input = {
        async read(limit) {
          if (offset === bytes.length) return undefined;
          const next = bytes.subarray(offset, offset + limit);
          offset += next.length;
          return next;
        },
      };
    } else this.#input = streams.stdin;
    const output = (name: "stdout" | "stderr") => {
      const selected = configuration[name];
      if (selected === "discard") return undefined;
      const stream = streams[name];
      if (typeof selected === "object" && !stream)
        throw new Error(`Browser ${name} files require an injected stdio provider`);
      return stream;
    };
    const stdout = output("stdout"),
      stderr = output("stderr");
    const environment = configuration.environment?.map(([name, value]) => [name, value]);
    const arguments_ = [...(configuration.arguments ?? [])];
    const cwd = configuration.initialCwd;
    this.methods = {
      "wasi:cli/environment@0.3.0#getEnvironment": () =>
        environment?.map((pair) => [...pair]) ?? [],
      "wasi:cli/environment@0.3.0#getArguments": () => [...arguments_],
      "wasi:cli/environment@0.3.0#getInitialCwd": () => cwd,
      "wasi:cli/stdin@0.3.0#readViaStream": ({ host }) => this.read(host),
      "wasi:cli/stdout@0.3.0#writeViaStream": ({ host }, [input]: [InputStream]) =>
        this.write(host, stdout, input),
      "wasi:cli/stderr@0.3.0#writeViaStream": ({ host }, [input]: [InputStream]) =>
        this.write(host, stderr, input),
      "wasi:cli/terminal-stdin@0.3.0#getTerminalStdin": () => undefined,
      "wasi:cli/terminal-stdout@0.3.0#getTerminalStdout": () => undefined,
      "wasi:cli/terminal-stderr@0.3.0#getTerminalStderr": () => undefined,
      "wasi:cli/exit@0.3.0#exit": (_call, [status]: [{ tag: string }]) =>
        this.exit(status.tag === "ok" ? 0 : 1),
      "wasi:cli/exit@0.3.0#exitWithCode": (_call, [status]: [number]) => this.exit(status),
    };
  }

  private exit(status: number): never {
    throw new GuestTrap(`orb exited with status ${status}; revision retired`);
  }

  private read(host: StandardServices): [OutputStream, HostFuture] {
    let settle!: (value: unknown) => void;
    const completed = new Promise((resolve) => {
      settle = resolve;
    });
    const dropped = new AbortController();
    const signal = combinedSignal(this.#stopped.signal, dropped.signal);
    const stream = new OutputStream(
      {
        next: async (limit) => {
          // Serialize across all stdin streams; a cancelled physical read still owns its turn.
          const operation = this.#reading.then(() =>
            host.physical(signal, async () => {
              const bytes = await this.#input?.read(limit, signal);
              if (bytes !== undefined && (!(bytes instanceof Uint8Array) || bytes.length > limit))
                throw new Error("stdin provider returned an invalid or oversized byte chunk");
              return bytes;
            }),
          );
          this.#reading = operation.then(
            () => {},
            () => {},
          );
          return operation;
        },
        close: () => dropped.abort(),
      },
      (failure) => settle(failure === undefined ? ok() : io()),
    );
    return [stream, new HostFuture(completed)];
  }

  private write(
    host: StandardServices,
    sink: StdioOutput | undefined,
    input: InputStream,
  ): HostFuture {
    const value = (async () => {
      try {
        for (let bytes = await input.read(); bytes; bytes = await input.read()) {
          if (sink) {
            const chunk = bytes;
            const operation = (this.#writes.get(sink) ?? Promise.resolve()).then(() =>
              host.physical(this.#stopped.signal, () => sink.write(chunk, this.#stopped.signal)),
            );
            const turn = operation.then(
              () => {},
              () => {},
            );
            this.#writes.set(sink, turn);
            try {
              await operation;
            } finally {
              if (this.#writes.get(sink) === turn) this.#writes.delete(sink);
            }
          }
        }
        return input.cancelled ? io() : ok();
      } catch {
        return io();
      } finally {
        input.cancel();
      }
    })();
    host.cleanup(() => value);
    return new HostFuture(value, () => input.cancel());
  }

  /** Request cancellation first; physical promises remain accounted for until they return. */
  retire(): void {
    this.#stopped.abort();
  }

  /** Close providers only after their physical operations have returned. */
  close(host: StandardServices): void {
    if (this.#closed) return;
    this.#closed = true;
    for (const provider of this.#providers)
      if (provider.close) host.cleanup(() => provider.close!());
  }
}
