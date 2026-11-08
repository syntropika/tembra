import * as cli from "@bytecodealliance/preview3-shim/cli";
import * as clocks from "@bytecodealliance/preview3-shim/clocks";
import * as filesystem from "@bytecodealliance/preview3-shim/filesystem";
import * as sockets from "@bytecodealliance/preview3-shim/sockets";
import type { OrbDefinition } from "../authoring/types.ts";
import { materialize, type AdaptedOrb } from "@syntropika/tembra-sdk/internal/adapters";
import { BrowserPort, type BrowserGuestModule } from "./browser-port.ts";
import type { FilesystemProvider } from "./filesystem.ts";
import { Revision } from "./revision.ts";
import { kebab, query, type ContractPort, type InterpreterOrbOptions } from "./types.ts";

export interface BrowserInterpreterOptions {
  engine: string | URL;
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
interface ContractModule {
  WasmContract: new (bytes: Uint8Array, world?: string) => ContractPort;
  default(input?: unknown): Promise<unknown>;
}

/** A browser backend uses per-instance namespaces; global WASI provider setters are never used. */
export class BrowserInterpreter {
  #instances = 0;
  readonly #core = new Map<string, Promise<WebAssembly.Module>>();

  /** Keep the optional parser and generated trusted engine out of metadata-only hosts. */
  constructor(
    readonly options: BrowserInterpreterOptions,
    readonly module: BrowserGuestModule,
    readonly contracts: ContractModule,
  ) {}

  /** Capture installable source independently of a host or a particular installation's VM. */
  orb(options: InterpreterOrbOptions): OrbDefinition {
    const captured = JSON.parse(JSON.stringify(options)) as InterpreterOrbOptions;
    return Object.freeze({
      name: captured.name,
      apiVersion: captured.apiVersion,
      [materialize]: () => this.open(captured),
    } as AdaptedOrb);
  }

  /** Decode contracts and check grants before creating any guest authority or executable heap. */
  private async open(options: InterpreterOrbOptions): Promise<OrbDefinition> {
    if (options.source.format !== "tembra-interpreter-source-v1")
      throw new Error("Unsupported prepared source format");
    const digest = await crypto.subtle.digest(
      "SHA-256",
      new TextEncoder().encode(options.source.source),
    );
    if (
      [...new Uint8Array(digest)].map((v) => v.toString(16).padStart(2, "0")).join("") !==
      options.source.sha256
    )
      throw new Error("Prepared source digest differs");
    const contract = new this.contracts.WasmContract(
      Uint8Array.from(options.source.contract),
      options.source.world,
    );
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
      (packet) => {
        const record = JSON.parse(packet);
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
      const bindings = options.bindings ?? {};
      query(contract, { kind: "profile", bindings });
      const plan = query<{ hostImports: { contract: string; provider: string }[] }>(contract, {
        kind: "plan",
        name: options.name,
        apiVersion: options.apiVersion,
        bindings,
      });
      const imports: Record<string, any> = {};
      for (const [package_, module] of Object.entries({ cli, clocks, filesystem, sockets }))
        for (const [name, value] of Object.entries(module))
          if (!name.startsWith("_")) imports[`wasi:${package_}/${kebab(name)}`] = value;
      for (const required of plan.hostImports) {
        if (
          ["wasi:filesystem/types@0.3.0", "wasi:filesystem/preopens@0.3.0"].includes(
            required.contract,
          )
        ) {
          if (!options.grants?.filesystem?.some((grant) => grant.provider === required.provider))
            throw new Error("Filesystem import has no Orb grant");
        } else if (
          required.contract !== "wasi:clocks/system-clock@0.3.0" ||
          required.provider !== "system-clock"
        )
          throw new Error(`Unsupported browser host contract ${required.contract}`);
      }
      if (options.grants?.network?.length)
        throw new Error("The browser host has no raw TCP provider");
      const mounts = new Set<string>();
      for (const grant of options.grants?.filesystem ?? []) {
        const provider = this.options.providers?.filesystem?.[grant.provider];
        if (
          !provider ||
          !plan.hostImports.some(
            (item) =>
              item.provider === grant.provider && item.contract.startsWith("wasi:filesystem/"),
          )
        )
          throw new Error("Filesystem grants require a configured provider and WIT import");
        if (
          !grant.mount.startsWith("/") ||
          grant.mount === "/" ||
          grant.mount
            .split("/")
            .slice(1)
            .some((part) => !part || part === "." || part === "..") ||
          mounts.has(grant.mount)
        )
          throw new Error("Filesystem mount must be absolute, normalized and unique");
        mounts.add(grant.mount);
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
      imports["wasi:sockets/types"] = {
        ...sockets.types,
        TcpSocket: class extends sockets.types.TcpSocket {
          static create = denied;
        },
        UdpSocket: class extends sockets.types.UdpSocket {
          static create = denied;
        },
      };
      imports["host-sync"] = { default: (packet: string) => port.sync(packet) };
      imports["host-async"] = { default: (packet: string) => port.async(packet) };
      imports["tembra:runtime/replies"] = { take: (ticket: number) => port.take(ticket) };
      port.guest = await this.module.instantiate((name) => {
        const url = new URL(name, this.options.engine).href;
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
          source: options.source.source,
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
      } finally {
        contract.free?.();
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
export async function createInterpreter(
  options: BrowserInterpreterOptions,
): Promise<BrowserInterpreter> {
  const wasm = WebAssembly as typeof WebAssembly & { promising?: unknown; Suspending?: unknown };
  if (!wasm.promising || !wasm.Suspending)
    throw new Error("This browser does not support WebAssembly JSPI");
  const engine = new URL(options.engine, globalThis.location?.href);
  const [module, contracts] = await Promise.all([
    import(engine.href) as Promise<BrowserGuestModule>,
    import("@syntropika/tembra-engine-wasm/contracts") as Promise<ContractModule>,
  ]);
  await contracts.default(options.contractsInput);
  return new BrowserInterpreter({ ...options, engine }, module, contracts);
}
