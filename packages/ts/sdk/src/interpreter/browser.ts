import * as io from "@bytecodealliance/preview2-shim/io";
import * as cli from "@bytecodealliance/preview2-shim/cli";
import * as clocks from "@bytecodealliance/preview2-shim/clocks";
import * as filesystem from "@bytecodealliance/preview2-shim/filesystem";
import * as sockets from "@bytecodealliance/preview2-shim/sockets";
import { materialize, type AdaptedOrb } from "#tembra-adapters";
import type { OrbDefinition, OrbArtifact, LoadOptions } from "../authoring/types.ts";
import { BrowserPort, type BrowserGuestModule } from "./browser-port.ts";
import type { FilesystemProvider } from "./filesystem.ts";
import { Revision } from "./revision.ts";
import { kebab, query, type ContractPort, type ProviderBinding } from "./types.ts";

export interface BrowserInterpreterOptions {
  engine?: string | URL;
  providers?: { filesystem?: Readonly<Record<string, FilesystemProvider>> };
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
}
interface ContractModule {
  WasmArtifact: new (bytes: Uint8Array) => BrowserArtifact;
  default(input?: unknown): Promise<unknown>;
}

/** A browser backend uses per-instance namespaces; global WASI provider setters are never used. */
export class BrowserExecution {
  #instances = 0;
  readonly #core = new Map<string, Promise<WebAssembly.Module>>();

  /** Keep the optional parser and generated trusted engine out of metadata-only hosts. */
  constructor(
    readonly options: BrowserInterpreterOptions,
    readonly module: BrowserGuestModule,
    readonly contracts: ContractModule,
  ) {}

  /** Decode data once while keeping every installation's executable heap separate. */
  decode(bytes: Uint8Array, options: LoadOptions = {}): OrbDefinition {
    const artifact = new this.contracts.WasmArtifact(bytes);
    const identity = JSON.parse(artifact.description()) as {
      name: string;
      apiVersion: number;
      kind: string;
    };
    if (identity.kind !== "source")
      throw new Error("Compiled Orb execution is unavailable in this browser backend");
    const captured = structuredClone(options);
    return Object.freeze({
      name: identity.name,
      apiVersion: identity.apiVersion,
      [materialize]: () => this.open(artifact, captured),
    } as AdaptedOrb);
  }

  /** Materialize bytes supplied directly through a common Orb declaration. */
  async materialize(definition: OrbArtifact): Promise<OrbDefinition> {
    return this.open(new this.contracts.WasmArtifact(definition.artifact), definition);
  }

  /** Check installation authority before creating any guest heap or filesystem descriptors. */
  private async open(contract: BrowserArtifact, installation: LoadOptions): Promise<OrbDefinition> {
    const identity = JSON.parse(contract.description()) as {
      name: string;
      apiVersion: number;
      kind: string;
    };
    if (identity.kind !== "source")
      throw new Error("Compiled Orb execution is unavailable in this browser backend");
    const bindings = query<Record<string, ProviderBinding>>(contract, {
      kind: "bindings",
      requires: installation.requires ?? {},
      grants: installation.grants ?? {},
    });
    const options = { ...identity, bindings, grants: installation.grants };
    const mounted: {
      types: unknown;
      preopens: { getDirectories(): [unknown, string][] };
      dispose(): void | Promise<void>;
    }[] = [];
    let counted = false;
    const port = new BrowserPort(
      this.options.limits?.timeoutMs ?? 10000,
      async () => {
        const settled = await Promise.allSettled(
          mounted.map(async (filesystem) => filesystem.dispose()),
        );
        if (counted) {
          this.#instances--;
          counted = false;
        }
        const failures = settled.filter((result) => result.status === "rejected");
        if (failures.length)
          throw new AggregateError(
            failures.map((result) => result.reason),
            "Filesystem disposal failed",
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
      query(contract, { kind: "profile", bindings });
      const plan = query<{ hostImports: { contract: string; provider: string }[] }>(contract, {
        kind: "plan",
        name: options.name,
        apiVersion: options.apiVersion,
        bindings,
      });
      const imports: Record<string, any> = {};
      for (const [package_, module] of Object.entries({ cli, clocks, filesystem, sockets, io }))
        for (const [name, value] of Object.entries(module))
          if (!name.startsWith("_")) imports[`wasi:${package_}/${kebab(name)}`] = value;
      if (options.grants?.network?.length)
        throw new Error("This browser host has no raw TCP provider");
      query(contract, {
        kind: "grants",
        imports: plan.hostImports,
        grants: options.grants ?? {},
        filesystem: Object.keys(this.options.providers?.filesystem ?? {}),
        network: [],
      });
      for (const grant of options.grants?.filesystem ?? []) {
        const provider = this.options.providers!.filesystem![grant.provider]!;
        mounted.push(provider.open(grant.mount, grant.writable));
      }
      imports["wasi:filesystem/preopens"] = {
        getDirectories: () =>
          mounted
            .flatMap((filesystem) => filesystem.preopens.getDirectories())
            .map(([descriptor, mount]) => [port.io.descriptor(descriptor as object), mount]),
      };
      imports["wasi:cli/environment"] = {
        getEnvironment: () => [],
        getArguments: () => [],
        getInitialCwd: () => undefined,
      };
      const denied = () => {
        throw { payload: { tag: "access-denied" } };
      };
      imports["wasi:cli/exit"] = { exit: denied, exitWithCode: denied };
      imports["wasi:sockets/tcp-create-socket"] = { createTcpSocket: denied };
      imports["wasi:sockets/udp-create-socket"] = { createUdpSocket: denied };
      imports["host-sync"] = { default: (packet: string) => port.sync(packet) };
      port.guest = await this.module.instantiate((name) => {
        const url = new URL(name, this.options.engine!).href;
        let compiled = this.#core.get(url);
        if (!compiled) {
          compiled = fetch(url).then(async (response) => {
            if (!response.ok) throw new Error("Engine module request failed");
            return WebAssembly.compile(await response.arrayBuffer());
          });
          this.#core.set(url, compiled);
          compiled.catch(() => {
            if (this.#core.get(url) === compiled) this.#core.delete(url);
          });
        }
        return compiled;
      }, imports);
      const memory = this.options.limits?.memoryBytes ?? 128 << 20;
      if (!Number.isSafeInteger(memory) || memory < 2 << 20 || memory > 0xffffffff)
        throw new Error("Invalid interpreter memory bound");
      await port.load(
        JSON.stringify({
          source: contract.source(),
          heap_bytes: Math.floor(memory / 2),
          instruction_ticks: this.options.limits?.instructionTicks ?? 10000,
          contract: query(contract, { kind: "describe" }),
        }),
      );
      this.#instances++;
      counted = true;
      return new Revision(port, contract, this.options.limits?.timeoutMs ?? 10000).definition(
        options,
      );
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
  /** Count physically retained interpreter instances. */
  instances(): number {
    return this.#instances;
  }
}

/** Load the optional trusted interpreter assets and shared WIT parser behind Tembra's facade. */
export async function createExecution(
  options: BrowserInterpreterOptions,
): Promise<BrowserExecution> {
  const wasm = WebAssembly as typeof WebAssembly & { promising?: unknown; Suspending?: unknown };
  if (!wasm.promising || !wasm.Suspending)
    throw new Error("This browser does not support WebAssembly JSPI");
  const engine = new URL(
    options.engine ?? import.meta.resolve("@syntropika/tembra-engine-wasm/interpreter"),
    globalThis.location?.href,
  );
  const [module, contracts] = await Promise.all([
    import(engine.href) as Promise<BrowserGuestModule>,
    import("@syntropika/tembra-engine-wasm/contracts") as Promise<ContractModule>,
  ]);
  await contracts.default(options.contractsInput);
  return new BrowserExecution({ ...options, engine }, module, contracts);
}
