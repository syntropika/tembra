/**
 * WASI for trusted JCO bindings.
 *
 * Two separate surfaces:
 * - `runtimeInternals`: the WASI 0.2 interfaces the QuickJS runtime's C library imports in a
 *   source component. They come from a fixed private context that never sees installation grants
 *   or CLI data: no environment, arguments or preopens, empty stdin, discarded stdout/stderr
 *   (console output goes through the attributed `tembra:orb/host.log`), clocks frozen at zero
 *   unless the installation holds the clocks grant, and a deterministic random byte source that is
 *   never security material. This mirrors the native executor.
 * - `publicWasi`: the WASI 0.3 interfaces the public contract imports, served from the
 *   installation's granted providers with JCO's component-model async conventions (the same
 *   conventions as `@bytecodealliance/preview3-shim`, whose global singletons cannot carry
 *   per-installation grants).
 */
import type { CliOptions } from "../../authoring/types.ts";
import type { StdioStreams } from "../standard-cli.ts";
import type { ClockProvider, RandomProvider } from "../standard-system.ts";
import { filesystemImports, type StandardMount } from "./wasi-filesystem.ts";
import { guestBytes, hostBytes, type WasiHost } from "./wasi-host.ts";
import { httpImports, type HttpClient } from "./wasi-http.ts";
import { socketImports, type SocketBindings } from "./wasi-sockets.ts";

/** The WASI 0.2 interfaces a source component's runtime may import (`RUNTIME_INTERNALS`). */
const INTERNALS = new Set([
  "wasi:cli/environment",
  "wasi:cli/exit",
  "wasi:cli/stdin",
  "wasi:cli/stdout",
  "wasi:cli/stderr",
  "wasi:cli/terminal-input",
  "wasi:cli/terminal-output",
  "wasi:cli/terminal-stdin",
  "wasi:cli/terminal-stdout",
  "wasi:cli/terminal-stderr",
  "wasi:clocks/wall-clock",
  "wasi:clocks/monotonic-clock",
  "wasi:filesystem/types",
  "wasi:filesystem/preopens",
  "wasi:io/error",
  "wasi:io/poll",
  "wasi:io/streams",
  "wasi:random/random",
]);

/** Whether an exact import name is one of the runtime internals (`runtime_internal` in Rust). */
export function runtimeInternal(name: string): boolean {
  const at = name.indexOf("@0.2.");
  return at > 0 && INTERNALS.has(name.slice(0, at)) && /^[0-9]+$/.test(name.slice(at + 5));
}

/** A trap: JCO rethrows host `Error`s without a `payload`, which ends the instance. */
class RuntimeTrap extends Error {}

const ok = (val?: unknown) => (val === undefined ? { tag: "ok" } : { tag: "ok", val });

/** The fixed private WASI 0.2 context of one source component instance. */
export function runtimeInternals(
  names: readonly string[],
  clocks: ClockProvider | undefined,
  /** Bounded diagnostic sink for the runtime's stderr (panic text); never a log or authority. */
  panic?: (bytes: Uint8Array) => void,
): Record<string, unknown> {
  class Pollable {
    ready() {
      return true;
    }
    block() {}
  }
  class IoError {
    toDebugString() {
      return "runtime stream error";
    }
  }
  class InputStream {
    read() {
      return { tag: "err", val: { tag: "closed" } };
    }
    blockingRead() {
      return { tag: "err", val: { tag: "closed" } };
    }
    skip() {
      return { tag: "err", val: { tag: "closed" } };
    }
    blockingSkip() {
      return { tag: "err", val: { tag: "closed" } };
    }
    subscribe() {
      return new Pollable();
    }
  }
  /** Discarded output: console output is attributed through `host.log` instead. */
  class OutputStream {
    checkWrite() {
      return ok(65536n);
    }
    write() {
      return ok();
    }
    blockingWriteAndFlush() {
      return ok();
    }
    flush() {
      return ok();
    }
    blockingFlush() {
      return ok();
    }
    writeZeroes() {
      return ok();
    }
    blockingWriteZeroesAndFlush() {
      return ok();
    }
    subscribe() {
      return new Pollable();
    }
  }
  /** No preopens exist, so no descriptor is ever issued. */
  class Descriptor {}
  class DirectoryEntryStream {}
  class TerminalInput {}
  class TerminalOutput {}
  // A fixed xorshift sequence: deterministic, explicitly not randomness.
  let state = 0x9e3779b97f4a7c15n;
  const next = () => {
    state ^= (state << 13n) & 0xffff_ffff_ffff_ffffn;
    state ^= state >> 7n;
    state ^= (state << 17n) & 0xffff_ffff_ffff_ffffn;
    return state;
  };
  const stdin = new InputStream();
  const stdout = new OutputStream();
  class PanicStream extends OutputStream {
    override write(contents?: Uint8Array) {
      if (contents) panic?.(contents);
      return ok();
    }
    override blockingWriteAndFlush(contents?: Uint8Array) {
      if (contents) panic?.(contents);
      return ok();
    }
  }
  const stderr = new PanicStream();
  const interfaces: Record<string, Record<string, unknown>> = {
    "wasi:cli/environment": {
      getEnvironment: () => [],
      getArguments: () => [],
      initialCwd: () => undefined,
    },
    "wasi:cli/exit": {
      exit: () => {
        throw new RuntimeTrap("The Orb runtime requested process exit");
      },
      exitWithCode: () => {
        throw new RuntimeTrap("The Orb runtime requested process exit");
      },
    },
    "wasi:cli/stdin": { getStdin: () => stdin },
    "wasi:cli/stdout": { getStdout: () => stdout },
    "wasi:cli/stderr": { getStderr: () => stderr },
    "wasi:cli/terminal-input": { TerminalInput },
    "wasi:cli/terminal-output": { TerminalOutput },
    "wasi:cli/terminal-stdin": { getTerminalStdin: () => undefined },
    "wasi:cli/terminal-stdout": { getTerminalStdout: () => undefined },
    "wasi:cli/terminal-stderr": { getTerminalStderr: () => undefined },
    "wasi:clocks/wall-clock": {
      now: () => (clocks ? clocks.systemNow() : { seconds: 0n, nanoseconds: 0 }),
      resolution: () => ({ seconds: 0n, nanoseconds: 1_000_000 }),
    },
    "wasi:clocks/monotonic-clock": {
      now: () => (clocks ? clocks.monotonicNow() : 0n),
      resolution: () => 1_000_000n,
      subscribeInstant: () => new Pollable(),
      subscribeDuration: () => new Pollable(),
    },
    "wasi:filesystem/types": {
      Descriptor,
      DirectoryEntryStream,
      filesystemErrorCode: () => undefined,
    },
    "wasi:filesystem/preopens": { getDirectories: () => [] },
    "wasi:io/error": { Error: IoError },
    "wasi:io/poll": {
      Pollable,
      poll: (list: unknown[]) => Uint32Array.from(list.keys()),
    },
    "wasi:io/streams": { InputStream, OutputStream },
    "wasi:random/random": {
      getRandomBytes: (length: bigint) => {
        const bytes = new Uint8Array(Number(length > 65536n ? 65536n : length));
        for (let index = 0; index < bytes.length; index++) bytes[index] = Number(next() & 0xffn);
        return bytes;
      },
      getRandomU64: () => next(),
    },
  };
  const imports: Record<string, unknown> = {};
  for (const name of names) {
    if (!runtimeInternal(name)) continue;
    imports[name] = interfaces[name.slice(0, name.indexOf("@"))];
  }
  return imports;
}

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

/** Interfaces with a browser implementation; anything else imported is refused at install. */
const PUBLIC = new Set([
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
  const missing = names.filter(
    (name) => name.startsWith("wasi:") && !runtimeInternal(name) && !PUBLIC.has(name),
  );
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
  const write = (name: "stdout" | "stderr") => async (data: unknown) => {
    const output = need(cli, "cli").stdio[name];
    const selected = cli!.options[name];
    try {
      for await (const chunk of guestBytes(data, signal))
        if (output && selected !== "discard") {
          const bytes = chunk.slice();
          await host.physical(() => output.write(bytes, signal));
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
            return input ? await host.physical(() => input.read(limit, signal)) : undefined;
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
