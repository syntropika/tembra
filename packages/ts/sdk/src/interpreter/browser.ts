import type { OrbDefinition, OrbArtifact, LoadOptions } from "../authoring/types.ts";
import { BrowserPort } from "./browser-port.ts";
import { checkInterpreter, CoreGuest } from "./core.ts";
import type { BrowserNetworkProviders } from "./browser-network.ts";
import type { FilesystemProvider } from "./filesystem.ts";
import { BrowserCapabilities } from "./io.ts";
import { Revision } from "./revision.ts";
import { StandardHost } from "./standard-host.ts";
import type { SystemProviders } from "./standard-system.ts";
import { CLOCK_MONOTONIC } from "./abi.ts";
import type { StdioProvider, StdioStreams } from "./standard-cli.ts";
import { query, type ContractPort, type InstallationPlan } from "./types.ts";

export interface BrowserInterpreterOptions {
  /** URL of the interpreter core module; defaults to the packaged `interpreter.wasm`. */
  engine?: string | URL;
  providers?: BrowserNetworkProviders &
    SystemProviders & {
      filesystem?: Readonly<Record<string, FilesystemProvider>>;
      stdio?: StdioProvider;
    };
  limits?: {
    memoryBytes?: number;
    timeoutMs?: number;
    instructionTicks?: number;
    packetBytes?: number;
  };
  onLog?: (record: { orb: string; level: string; message: string }) => void;
  contractsInput?: unknown;
}
interface BrowserArtifact extends ContractPort {
  description(): string;
  source(): string;
  free(): void;
}
interface ContractModule {
  WasmArtifact: new (bytes: Uint8Array) => BrowserArtifact;
  default(input?: unknown): Promise<unknown>;
}

/** Compile the trusted interpreter once; a failed request may be retried by a later Orb. */
async function compileEngine(url: URL): Promise<WebAssembly.Module> {
  const response = await fetch(url);
  if (!response.ok) throw new Error("Engine module request failed");
  const module = await WebAssembly.compile(await response.arrayBuffer());
  checkInterpreter(module);
  return module;
}

/** A browser backend instantiates one isolated core module, memory and heap per Orb. */
export class BrowserExecution {
  #engine: Promise<WebAssembly.Module> | undefined;

  /** Keep the prepared-contract validator and trusted engine out of metadata-only hosts. */
  constructor(
    readonly options: BrowserInterpreterOptions,
    readonly contracts: ContractModule,
  ) {}

  /** Share compiled code between installations; every instance still owns its state. */
  private engine(): Promise<WebAssembly.Module> {
    if (!this.#engine) {
      const compiled = compileEngine(new URL(this.options.engine!));
      this.#engine = compiled;
      compiled.catch(() => {
        if (this.#engine === compiled) this.#engine = undefined;
      });
    }
    return this.#engine;
  }

  /** Own one checked contract for exactly one installed revision. */
  async materialize(definition: OrbArtifact): Promise<OrbDefinition> {
    const artifact = new this.contracts.WasmArtifact(definition.artifact);
    let released = false;
    const release = () => {
      if (released) return;
      released = true;
      artifact.free();
    };
    try {
      return await this.open(artifact, definition, release);
    } catch (error) {
      release();
      throw error;
    }
  }

  /** Check installation authority before creating any guest heap or filesystem descriptors. */
  private async open(
    contract: BrowserArtifact,
    installation: LoadOptions,
    release: () => void,
  ): Promise<OrbDefinition> {
    const identity = JSON.parse(contract.description()) as {
      name: string;
      apiVersion: number;
      kind: string;
    };
    if (identity.kind !== "source")
      throw new Error("Compiled Orb execution is unavailable in this browser backend");
    const prepared = query<InstallationPlan>(contract, {
      kind: "install",
      name: identity.name,
      apiVersion: identity.apiVersion,
      requires: installation.requires ?? {},
      grants: installation.grants ?? {},
      filesystem: Object.keys(this.options.providers?.filesystem ?? {}),
      network: [
        ...Object.keys(this.options.providers?.network ?? {}),
        ...Object.keys(this.options.providers?.http ?? {}),
      ],
    });
    const options = { ...identity, grants: installation.grants };
    let capabilities: BrowserCapabilities | undefined;
    let stdio: StdioStreams | undefined;
    const port = new BrowserPort(
      this.options.limits?.timeoutMs ?? 10000,
      async () => {
        const cleanup: Promise<unknown>[] = [Promise.resolve().then(() => capabilities?.dispose())];
        // Until construction succeeds, this scope owns providers returned by the factory.
        if (stdio)
          for (const provider of new Set([stdio.stdin, stdio.stdout, stdio.stderr]))
            cleanup.push(Promise.resolve().then(() => provider?.close?.()));
        const settled = await Promise.allSettled(cleanup);
        release();
        const failed = settled.filter(
          (result): result is PromiseRejectedResult => result.status === "rejected",
        );
        if (failed.length)
          throw new AggregateError(
            failed.map((result) => result.reason),
            "Provider cleanup failed",
          );
      },
      (record) => {
        if (!["log", "debug", "info", "warn", "error"].includes(record.level))
          throw new Error("Invalid console level");
        try {
          this.options.onLog?.({
            orb: options.name,
            level: record.level,
            message: String(record.message),
          });
        } catch {
          /* Logging observers do not control the guest. */
        }
      },
      this.options.limits?.packetBytes ?? 8 << 20,
    );
    try {
      const memory = this.options.limits?.memoryBytes ?? 128 << 20;
      if (!Number.isSafeInteger(memory) || memory < 2 << 20 || memory > 0xffffffff)
        throw new Error("Invalid interpreter memory bound");
      capabilities = await BrowserCapabilities.open(
        options.grants ?? {},
        this.options.providers ?? {},
      );
      const description = query<{ imports: { contract: string; type_only: boolean }[] }>(contract, {
        kind: "describe",
      });
      if (options.grants?.system?.includes("cli"))
        stdio = this.options.providers?.stdio?.(options.name, installation.cli ?? {});
      // Standard WASI calls reach only the interfaces this revision imported and was granted.
      port.standard = new StandardHost(
        new Set(
          description.imports
            .filter((item) => item.type_only === false)
            .map((item) => item.contract),
        ),
        capabilities.mounts,
        this.options.limits?.packetBytes ?? 8 << 20,
        {
          clocks: this.options.providers?.clocks,
          random: this.options.providers?.random,
          ...(options.grants?.system?.includes("cli")
            ? {
                cli: installation.cli,
                stdio,
              }
            : { stdio: undefined }),
        },
      );
      // The standard host now owns provider cleanup, including failed VM initialization.
      stdio = undefined;
      const { clocks, random } = port.standard;
      // The instance links no filesystem, socket or environment capability.
      port.guest = await CoreGuest.create(await this.engine(), memory, {
        sync: (packet) => (typeof packet === "string" ? port.sync(packet) : port.sync(packet)),
        log: (level, message) => port.log({ operation: "console", level, message }),
        clock: (clock) => {
          if (!options.grants?.system?.includes("clocks")) return 0n;
          if (clock === CLOCK_MONOTONIC) return clocks.monotonicNow();
          const instant = clocks.systemNow();
          return instant.seconds * 1_000_000_000n + BigInt(instant.nanoseconds);
        },
      });
      await port.load(
        JSON.stringify({
          source: contract.source(),
          heap_bytes: Math.floor(memory / 2),
          instruction_ticks: this.options.limits?.instructionTicks ?? 10000,
          contract: description,
          builtins: {
            clock: options.grants?.system?.includes("clocks") ?? false,
            seed: options.grants?.system?.includes("random")
              ? random.insecureSeed().map(String)
              : null,
          },
        }),
      );
      return new Revision(
        port,
        contract,
        this.options.limits?.timeoutMs ?? 10000,
        (request, signal) => capabilities!.execute(request, signal),
      ).definition(options, prepared);
    } catch (error) {
      try {
        await port.close();
      } catch (disposal) {
        throw new AggregateError(
          [error, disposal],
          "Interpreter initialization and disposal failed",
        );
      }
      throw error;
    }
  }
}

/** Load the optional interpreter assets and prepared-contract validator behind Tembra's facade. */
export async function createExecution(
  options: BrowserInterpreterOptions,
): Promise<BrowserExecution> {
  // The engine package locates its own interpreter, so bundled SDK code needs no import map.
  const engine =
    options.engine === undefined
      ? (await import("@syntropika/tembra-engine-wasm/assets")).interpreterURL()
      : new URL(options.engine, globalThis.location?.href);
  const contracts = (await import("@syntropika/tembra-engine-wasm/contracts")) as ContractModule;
  await contracts.default(options.contractsInput);
  return new BrowserExecution({ ...options, engine }, contracts);
}
