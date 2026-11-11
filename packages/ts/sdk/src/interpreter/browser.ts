import type { OrbDefinition, OrbArtifact, LoadOptions } from "../authoring/types.ts";
import type { BrowserNetworkProviders } from "./browser-network.ts";
import type { FilesystemProvider } from "./filesystem.ts";
import { BrowserCapabilities } from "./io.ts";
import type { StdioProvider, StdioStreams } from "./standard-cli.ts";
import {
  browserClocks,
  browserRandom,
  checkedClocks,
  revisionRandom,
  type SystemProviders,
} from "./standard-system.ts";
import { query, type ContractPort, type InstallationPlan } from "./types.ts";
import type { LogLevel } from "./host-protocol.ts";
import {
  type CoreLimits,
  type InstrumentedCore,
  requireJspi,
  TrustedCompiler,
} from "./component/compiler.ts";
import {
  dependencyImport,
  hookImport,
  HOST,
  orbHost,
  type ImportedFunction,
} from "./component/host.ts";
import { ComponentPort } from "./component/port.ts";
import { ComponentRevision, LIFECYCLE, type Described } from "./component/revision.ts";
import { converters, type GraphFunction } from "./component/values.ts";
import {
  checkPublicWasi,
  publicWasi,
  runtimeInternal,
  runtimeInternals,
} from "./component/wasi.ts";

export interface BrowserInterpreterOptions {
  providers?: BrowserNetworkProviders &
    SystemProviders & {
      filesystem?: Readonly<Record<string, FilesystemProvider>>;
      stdio?: StdioProvider;
    };
  limits?: {
    /**
     * Wall time of one guest callback, including its detached host work until physical
     * completion; the instance is quarantined when it is exceeded.
     */
    timeoutMs?: number;
    /**
     * Guest linear memory of one instance in bytes (whole 64 KiB pages): initial memories of
     * every core instantiation plus `memory.grow`. Tables, the GC heap and host buffers are not
     * counted.
     */
    memoryBytes?: number;
    /**
     * Fuel of one admitted callback, and separately of instantiation: function entries and loop
     * iterations, not instructions or time. Exhaustion traps and quarantines the revision.
     */
    fuel?: number;
  };
  onLog?: (record: { orb: string; level: string; message: string }) => void;
  contractsInput?: unknown;
}
interface BrowserArtifact extends ContractPort {
  description(): string;
  sourceRuntime(): boolean;
  requiresRuntime(): boolean;
  component(): Uint8Array;
  expandedComponent(runtime: SharedRuntime): Uint8Array;
  releaseComponent(): void;
  imports(): string;
  contributions(): string;
  free(): void;
}
interface SharedRuntime {
  free(): void;
}
interface ContractModule {
  WasmArtifact: new (bytes: Uint8Array) => BrowserArtifact;
  WasmSharedRuntime: new (bytes: Uint8Array) => SharedRuntime;
  instrumentCore(bytes: Uint8Array, maximumPages: number): InstrumentedCore;
  default(input?: unknown): Promise<unknown>;
}

/** Resolve only Tembra's packaged engine asset; an Orb cannot select a download URL. */
async function loadRuntime(): Promise<Uint8Array> {
  const { default: url } = await import("@syntropika/tembra-engine-wasm/runtime");
  const response = await fetch(url);
  if (!response.ok) throw new Error(`Shared runtime request failed: ${response.status}`);
  return new Uint8Array(await response.arrayBuffer());
}

const PAGE = 1 << 16;

/** A limit is a positive safe integer; nothing else is coerced. */
function positive(name: string, value: number): number {
  if (!Number.isSafeInteger(value) || value <= 0)
    throw new Error(`limits.${name} must be a positive safe integer`);
  return value;
}

const LEVELS: readonly string[] = ["log", "debug", "info", "warn", "error"];

/**
 * A browser backend: every Orb is a verified component, compiled by Tembra's trusted compiler and
 * instantiated once per installed revision with its own memories, tables and host tables.
 */
export class BrowserExecution {
  #runtime: Promise<SharedRuntime> | undefined;
  readonly #instrumentCore = (bytes: Uint8Array, pages: number) =>
    this.contracts.instrumentCore(bytes, pages);

  /** Keep the verifier and the lazily loaded trusted compiler out of metadata-only hosts. */
  constructor(
    readonly options: BrowserInterpreterOptions,
    readonly contracts: ContractModule,
    readonly compiler: TrustedCompiler = new TrustedCompiler(),
    private readonly runtimeInput: () => Promise<Uint8Array> = loadRuntime,
  ) {}

  /** Load and check immutable interpreter bytes once; every Orb still owns its mutable state. */
  private sharedRuntime(): Promise<SharedRuntime> {
    if (!this.#runtime) {
      const loading = this.runtimeInput().then(
        (bytes) => new this.contracts.WasmSharedRuntime(bytes),
      );
      this.#runtime = loading;
      loading.catch(() => {
        if (this.#runtime === loading) this.#runtime = undefined;
      });
    }
    return this.#runtime;
  }

  /** Own one verified artifact for exactly one installed revision. */
  async materialize(definition: OrbArtifact): Promise<OrbDefinition> {
    // Verification runs on the actual component types before any guest code exists.
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

  /** Check installation authority before creating any guest instance or provider. */
  private async open(
    contract: BrowserArtifact,
    installation: LoadOptions,
    release: () => void,
  ): Promise<OrbDefinition> {
    requireJspi();
    const identity = JSON.parse(contract.description()) as {
      name: string;
      apiVersion: number;
    };
    const source = contract.sourceRuntime();
    const names = JSON.parse(contract.imports()) as string[];
    checkPublicWasi(names);
    const prepared = query<InstallationPlan>(contract, {
      kind: "install",
      name: identity.name,
      apiVersion: identity.apiVersion,
      requires: installation.requires ?? {},
      grants: installation.grants ?? {},
      filesystem: Object.keys(this.options.providers?.filesystem ?? {}),
      network: Object.keys(this.options.providers?.network ?? {}),
      http: Object.keys(this.options.providers?.http ?? {}),
    });
    const graph = query<Described>(contract, { kind: "describe" });
    const timeout = positive("timeoutMs", this.options.limits?.timeoutMs ?? 10000);
    const memoryBytes = positive("memoryBytes", this.options.limits?.memoryBytes ?? 128 << 20);
    const fuel = BigInt(positive("fuel", this.options.limits?.fuel ?? 100_000_000));
    const maximumPages = Math.floor(memoryBytes / PAGE);
    if (maximumPages < 1 || maximumPages > 0xffff)
      throw new Error("limits.memoryBytes must hold between 1 and 65535 pages of 64 KiB");
    const grants = installation.grants ?? {};
    const system = grants.system ?? [];
    const ended = new AbortController();
    const runtime = contract.requiresRuntime() ? await this.sharedRuntime() : undefined;
    let capabilities: BrowserCapabilities | undefined;
    let stdio: StdioStreams | undefined;
    const port = new ComponentPort(
      timeout,
      async () => {
        ended.abort();
        const cleanup: Promise<unknown>[] = [Promise.resolve().then(() => capabilities?.dispose())];
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
      fuel,
    );
    try {
      capabilities = await BrowserCapabilities.open(grants, this.options.providers ?? {});
      if (system.includes("cli"))
        stdio = this.options.providers?.stdio?.(identity.name, installation.cli ?? {}) ?? {};
      const clocks = system.includes("clocks")
        ? checkedClocks(this.options.providers?.clocks ?? browserClocks())
        : undefined;
      const random = system.includes("random")
        ? revisionRandom(this.options.providers?.random ?? browserRandom())
        : undefined;
      const log = (level: LogLevel, message: string) => {
        if (!LEVELS.includes(level)) return;
        this.options.onLog?.({
          orb: identity.name,
          level,
          message: String(message),
        });
      };
      // Each socket interface uses exactly the provider its import binding selected.
      const bound = (name: string) => {
        const entry = graph.imports.find((item) => item.contract === name);
        const binding = entry && prepared.bindings[entry.binding];
        return binding?.kind === "host" ? binding.provider : undefined;
      };
      const convert = converters(graph);
      const revision = new ComponentRevision(port, contract, graph, timeout, source);
      const imports: Record<string, unknown> = {
        ...runtimeInternals(source ? names : [], clocks, (bytes) => port.panic(bytes)),
        ...publicWasi(names, {
          clocks,
          random,
          ...(stdio ? { cli: { options: installation.cli ?? {}, stdio } } : {}),
          mounts: () => capabilities?.mounts ?? [],
          host: port.wasi,
          http: capabilities.network,
          ingress: (bridge) => (revision.ingressBridge = bridge as never),
          sockets: {
            types: capabilities.network.sockets(bound("wasi:sockets/types@0.3.0")),
            lookup: capabilities.network.sockets(bound("wasi:sockets/ip-name-lookup@0.3.0")),
          },
        }),
      };
      const suspending: string[] = [];
      const promising: string[] = [];
      const functions = (entry: { functions?: Record<string, unknown> }): ImportedFunction[] =>
        Object.entries(entry.functions ?? {}).map(([name, fn]) => ({
          name,
          spelling: graph.javascriptNames[name] ?? name,
          values: convert(fn as GraphFunction),
        }));
      for (const entry of graph.exports)
        for (const name of Object.keys(entry.functions ?? {}))
          promising.push(`${entry.binding}#${name}`);
      for (const name of names) {
        if (Object.hasOwn(imports, name)) continue;
        if (name === HOST && source) {
          imports[name] = orbHost(port, log);
          continue;
        }
        if (runtimeInternal(name) && !source)
          throw new Error(`A compiled component may not import ${name}`);
        const selected = graph.imports.find((item) => item.binding === name);
        const service = selected && prepared.importServices[name];
        if (selected && service && prepared.bindings[name]?.kind === "orb") {
          imports[name] = dependencyImport(port, name, functions(selected));
          for (const fn of Object.keys(selected.functions ?? {})) suspending.push(`${name}#${fn}`);
          continue;
        }
        if (selected?.type_only) {
          imports[name] = {};
          continue;
        }
        const hook = source && !selected && graph.exports.find((item) => item.binding === name);
        if (hook) {
          const object: Record<string, unknown> = {};
          for (const fn of functions(hook))
            Object.assign(
              object,
              hookImport(port, () => revision.hookFor(name), fn),
            );
          imports[name] = object;
          for (const fn of Object.keys(hook.functions ?? {})) suspending.push(`${name}#${fn}`);
          continue;
        }
        throw new Error(`Browser component execution cannot provide import ${name}`);
      }
      const compiled = await this.compiler.compile({
        component: runtime ? contract.expandedComponent(runtime) : contract.component(),
        imports: names,
        suspendingImports: suspending,
        promisingExports: promising,
        instrumentCore: this.#instrumentCore,
        maximumPages,
      });
      contract.releaseComponent();
      // One budget per instance: pages are shared by all its core modules; instantiation runs on
      // its own finite fuel, and each admitted callback is refilled to `fuel`.
      const limits: CoreLimits = {
        fuel: new WebAssembly.Global({ value: "i64", mutable: true }, fuel),
        pages: new WebAssembly.Global({ value: "i64", mutable: true }, BigInt(maximumPages)),
      };
      port.limits = limits;
      const instance = await compiled.instantiate(imports, port.observer, limits);
      port.exports = instance as never;
      if (source && !instance[LIFECYCLE])
        throw new Error("The component exports no Tembra lifecycle");
      const description = await revision.describe(
        identity,
        JSON.parse(contract.contributions()) as Record<string, number>,
      );
      return revision.definition(identity, prepared, description);
    } catch (error) {
      try {
        await port.close();
      } catch (disposal) {
        throw new AggregateError([error, disposal], "Component initialization and disposal failed");
      }
      throw error;
    }
  }
}

/** Load the verifier behind Tembra's facade; the compiler loads on the first installation. */
export async function createExecution(
  options: BrowserInterpreterOptions,
): Promise<BrowserExecution> {
  const contracts =
    (await import("@syntropika/tembra-engine-wasm/contracts")) as unknown as ContractModule;
  await contracts.default(options.contractsInput);
  return new BrowserExecution(options, contracts);
}
