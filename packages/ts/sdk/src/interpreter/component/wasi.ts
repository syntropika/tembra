/**
 * WASI 0.3 for trusted prepared component bindings.
 *
 * `publicWasi` serves the WASI 0.3 interfaces a compiled component imports from the
 * installation's granted providers, with JCO's component-model async conventions (the same
 * conventions as `@bytecodealliance/preview3-shim`, whose global singletons cannot carry
 * per-installation grants). Every instance receives fresh import objects and resources.
 */
import type { CliOptions } from "../../authoring/types.ts";
import type { StdioOutput, StdioStreams } from "../standard-cli.ts";
import type { ClockProvider, RandomProvider } from "../standard-system.ts";
import { filesystemImports, type StandardMount } from "./wasi-filesystem.ts";
import { cancelled, guestBytes, hostBytes, type WasiHost } from "./wasi-host.ts";
import { httpImports, type HttpClient } from "./wasi-http.ts";
import { socketImports, type SocketBindings } from "./wasi-sockets.ts";

/** A trap: JCO rethrows host `Error`s without a `payload`, which ends the instance. */
class RuntimeTrap extends Error {}

const ok = (val?: unknown) => (val === undefined ? { tag: "ok" } : { tag: "ok", val });

/** Granted providers of one installation for the public WASI 0.3 surface. */
export interface PublicProviders {
  readonly clocks?: ClockProvider | undefined;
  readonly random?: RandomProvider | undefined;
  /** Present only with the `cli` grant. */
  readonly cli?: {
    readonly options: Readonly<CliOptions>;
    readonly stdio: StdioStreams;
  };
  /** Granted filesystem mounts, in grant order. */
  readonly mounts: () => readonly StandardMount[];
  /** Physical accounting and retirement of this instance. */
  readonly host: WasiHost;
  /** Receives the instance's incoming-HTTP bridge, when the HTTP adapter provides one. */
  readonly ingress?: (bridge: unknown) => void;
  /** Exact-origin HTTP grants; `undefined` denies every request. */
  readonly http: HttpClient | undefined;
  /** The socket provider and grants selected by each sockets import binding. */
  readonly sockets: SocketBindings;
}

/** Interfaces with a browser implementation; anything else imported is refused. */
export const BROWSER_WASI: ReadonlySet<string> = new Set([
  "wasi:clocks/monotonic-clock@0.3.0",
  "wasi:clocks/system-clock@0.3.0",
  "wasi:clocks/types@0.3.0",
  "wasi:random/random@0.3.0",
  "wasi:random/insecure@0.3.0",
  "wasi:random/insecure-seed@0.3.0",
  "wasi:cli/environment@0.3.0",
  "wasi:cli/exit@0.3.0",
  "wasi:cli/stdin@0.3.0",
  "wasi:cli/stdout@0.3.0",
  "wasi:cli/stderr@0.3.0",
  "wasi:cli/types@0.3.0",
  "wasi:cli/terminal-input@0.3.0",
  "wasi:cli/terminal-output@0.3.0",
  "wasi:cli/terminal-stdin@0.3.0",
  "wasi:cli/terminal-stdout@0.3.0",
  "wasi:cli/terminal-stderr@0.3.0",
  "wasi:filesystem/types@0.3.0",
  "wasi:filesystem/preopens@0.3.0",
  "wasi:http/types@0.3.0",
  "wasi:http/client@0.3.0",
  "wasi:sockets/types@0.3.0",
  "wasi:sockets/ip-name-lookup@0.3.0",
]);

/** Refuse, before instantiation, a public WASI import this backend cannot serve yet. */
export function checkPublicWasi(names: readonly string[]): void {
  const missing = names.filter((name) => name.startsWith("wasi:") && !BROWSER_WASI.has(name));
  if (missing.length)
    throw new Error(
      `Browser component execution does not provide ${missing.join(", ")} yet; ` +
        "this Orb cannot be installed in the browser",
    );
}

/** The granted public WASI 0.3 imports of one installation, keyed by exact import name. */
export function publicWasi(
  names: readonly string[],
  providers: PublicProviders,
): Record<string, unknown> {
  const { clocks, random, cli, host } = providers;
  const signal = host.retired;
  const need = <T>(value: T | undefined, grant: string): T => {
    if (value === undefined) throw new Error(`The installation holds no ${grant} grant`);
    return value;
  };
  const randomBytes = (secure: boolean) => (length: bigint) => {
    const source = need(random, "random");
    const size = Number(length > 1n << 20n ? 1n << 20n : length);
    const bytes = secure ? source.secure(size) : source.insecure(size);
    if (!(bytes instanceof Uint8Array) || bytes.length !== size)
      throw new Error("random provider returned the wrong byte length");
    return bytes;
  };
  const randomU64 = (secure: boolean) => () => {
    const bytes = randomBytes(secure)(8n);
    return new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, true);
  };
  let seed: readonly [bigint, bigint] | undefined;
  const writes = new Map<StdioOutput, Promise<void>>();
  const write = (name: "stdout" | "stderr") => async (data: unknown) => {
    const output = need(cli, "cli").stdio[name];
    const selected = cli!.options[name];
    const cancel = host.signal();
    try {
      for await (const chunk of guestBytes(data, cancel))
        if (output && selected !== "discard") {
          const bytes = chunk.slice();
          // One provider may back both output interfaces. A queued write retains its
          // initiating callback and physical ownership before it waits for the previous sink.
          const operation = (writes.get(output) ?? Promise.resolve()).then(() => {
            if (cancel.aborted) throw cancelled();
            return output.write(bytes, cancel);
          });
          const turn = operation.then(
            () => {},
            () => {},
          );
          writes.set(output, turn);
          void turn.then(() => {
            if (writes.get(output) === turn) writes.delete(output);
          });
          await host.physical(() => operation);
        }
      return ok();
    } catch {
      return { tag: "err", val: "io" };
    }
  };
  const waitUntil = async (when: bigint): Promise<void> => {
    const source = need(clocks, "clocks");
    const cancel = host.signal();
    try {
      await host.physical(() => source.waitUntil(when, cancel));
    } catch (error) {
      // WASI clock waits have no error lane. Like the native host, cancellation ends the
      // wait early; the caller observes its cancelled invocation independently.
      if (!cancel.aborted) throw error;
    }
  };
  const interfaces: Record<string, Record<string, unknown>> = {
    "wasi:clocks/types@0.3.0": {},
    "wasi:clocks/monotonic-clock@0.3.0": {
      now: () => need(clocks, "clocks").monotonicNow(),
      getResolution: () => need(clocks, "clocks").monotonicResolution(),
      // A wait belongs to the callback that started it: its cancellation, end or the
      // revision's retirement stops the provider wait, which stays physically accounted.
      waitUntil,
      waitFor: (duration: bigint) => waitUntil(need(clocks, "clocks").monotonicNow() + duration),
    },
    "wasi:clocks/system-clock@0.3.0": {
      now: () => need(clocks, "clocks").systemNow(),
      getResolution: () => need(clocks, "clocks").systemResolution(),
    },
    "wasi:random/random@0.3.0": {
      getRandomBytes: randomBytes(true),
      getRandomU64: randomU64(true),
    },
    "wasi:random/insecure@0.3.0": {
      getInsecureRandomBytes: randomBytes(false),
      getInsecureRandomU64: randomU64(false),
    },
    "wasi:random/insecure-seed@0.3.0": {
      // Stable for one instance, as the interface requires.
      getInsecureSeed: () => (seed ??= need(random, "random").insecureSeed()),
    },
    "wasi:cli/types@0.3.0": {},
    "wasi:cli/environment@0.3.0": {
      getEnvironment: () => (cli?.options.environment ?? []).map(([key, value]) => [key, value]),
      getArguments: () => [...(cli?.options.arguments ?? [])],
      getInitialCwd: () => cli?.options.initialCwd,
    },
    "wasi:cli/exit@0.3.0": {
      // Exit ends the revision: it is a trap carrying the exact status, never a guest result.
      exit: (status: { tag: string }) => {
        throw new RuntimeTrap(`orb exited with status ${status?.tag === "ok" ? 0 : 1}`);
      },
      exitWithCode: (code: number) => {
        throw new RuntimeTrap(`orb exited with status ${code}`);
      },
    },
    "wasi:cli/stdout@0.3.0": { writeViaStream: write("stdout") },
    "wasi:cli/stderr@0.3.0": { writeViaStream: write("stderr") },
    "wasi:cli/stdin@0.3.0": {
      readViaStream: () => {
        const options = need(cli, "cli").options.stdin;
        const input = cli!.stdio.stdin;
        let settle!: (value: unknown) => void;
        const result = new Promise((resolve) => (settle = resolve));
        const fixed =
          typeof options === "object" && "bytes" in options ? options.bytes.slice() : undefined;
        let offset = 0;
        const readable = hostBytes(
          async (limit) => {
            if (options === "empty") return undefined;
            if (fixed) {
              const chunk = fixed.subarray(offset, offset + limit);
              offset += chunk.length;
              return chunk.length ? chunk.slice() : undefined;
            }
            // A retained stdin stream can be read by a later callback. Capture that read's
            // cancellation now, rather than the callback that originally opened the stream.
            const cancel = host.signal();
            return input ? await host.physical(() => input.read(limit, cancel)) : undefined;
          },
          (end) => settle(end === "failed" ? { tag: "err", val: "io" } : ok()),
          signal,
        );
        return [readable, result];
      },
    },
    "wasi:cli/terminal-input@0.3.0": { TerminalInput: class TerminalInput {} },
    "wasi:cli/terminal-output@0.3.0": {
      TerminalOutput: class TerminalOutput {},
    },
    "wasi:cli/terminal-stdin@0.3.0": { getTerminalStdin: () => undefined },
    "wasi:cli/terminal-stdout@0.3.0": { getTerminalStdout: () => undefined },
    "wasi:cli/terminal-stderr@0.3.0": { getTerminalStderr: () => undefined },
  };
  const filesystem = filesystemImports(host, providers.mounts);
  interfaces["wasi:filesystem/types@0.3.0"] = filesystem.types;
  interfaces["wasi:filesystem/preopens@0.3.0"] = filesystem.preopens;
  const http = httpImports(host, providers.http);
  interfaces["wasi:http/types@0.3.0"] = http.types;
  interfaces["wasi:http/client@0.3.0"] = http.client;
  providers.ingress?.((http as { ingress?: unknown }).ingress);
  const sockets = socketImports(host, providers.sockets);
  interfaces["wasi:sockets/types@0.3.0"] = sockets.types;
  interfaces["wasi:sockets/ip-name-lookup@0.3.0"] = sockets["ip-name-lookup"];
  checkPublicWasi(names);
  const imports: Record<string, unknown> = {};
  for (const name of names) if (interfaces[name]) imports[name] = interfaces[name];
  return imports;
}
