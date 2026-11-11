/**
 * Installation of a compiled Orb through its application-owned prepared adapter.
 *
 * Loaded lazily: hosts running only source Orbs never fetch this code. The artifact's metadata
 * was accepted only because the browser verifier hashed its actual bytes to the adapter's digest
 * (see `prepared.ts`). Every installation receives a fresh instance: JCO state, memories,
 * tables, resources, WASI import objects, providers and limit globals.
 */
import type { LoadOptions, OrbDefinition } from "../../authoring/types.ts";
import type { BrowserNetworkProviders } from "../browser-network.ts";
import type { FilesystemProvider } from "../filesystem.ts";
import { BrowserCapabilities } from "../io.ts";
import type { StdioProvider, StdioStreams } from "../standard-cli.ts";
import {
  browserClocks,
  browserRandom,
  checkedClocks,
  revisionRandom,
  type SystemProviders,
} from "../standard-system.ts";
import { query, type ContractPort, type InstallationPlan } from "../types.ts";
import { referenceImport, referenceView, resourceImport } from "./host.ts";
import { instantiate, requireJspi, type CoreLimits } from "./instance.ts";
import { ComponentPort } from "./port.ts";
import type { PreparedRecord } from "./prepared.ts";
import { compiledDescription, ComponentRevision, type Described } from "./revision.ts";
import { resourceDataFunction, resourceRoles } from "./resources.ts";
import { converters, type GraphFunction } from "./values.ts";
import { checkPublicWasi, publicWasi } from "./wasi.ts";

/** Browser options a compiled installation reads. */
export interface ComponentOptions {
  providers?: BrowserNetworkProviders &
    SystemProviders & {
      filesystem?: Readonly<Record<string, FilesystemProvider>>;
      stdio?: StdioProvider;
    };
  limits?: {
    memoryBytes?: number;
    timeoutMs?: number;
    fuel?: number;
  };
}

/** The verifier's digest-bound metadata of one compiled artifact. */
export interface PreparedContract extends ContractPort {
  description(): string;
  contributions(): string;
  digest(): string;
}

const PAGE = 1 << 16;

/** A limit is a positive safe integer; nothing else is coerced. */
function positive(name: string, value: number): number {
  if (!Number.isSafeInteger(value) || value <= 0)
    throw new Error(`limits.${name} must be a positive safe integer`);
  return value;
}

/** Refuse an adapter whose generated JSPI wrapping does not cover what this revision calls. */
function checkAdapter(record: PreparedRecord, graph: Described, plan: InstallationPlan): void {
  for (const entry of graph.exports)
    for (const name of Object.keys(entry.functions ?? {}))
      if (!record.promising.has(`${entry.binding}#${name}`))
        throw new Error(`The prepared adapter does not wrap export ${entry.binding}#${name}`);
  for (const name of record.imports) {
    if (name.startsWith("wasi:")) continue;
    const selected = graph.imports.find((item) => item.binding === name);
    if (!selected) throw new Error(`The prepared adapter names unknown import ${name}`);
    if (selected.type_only) continue;
    if (plan.bindings[name]?.kind !== "orb" || !Object.hasOwn(plan.importServices, name))
      throw new Error(`Browser component execution cannot provide import ${name}`);
    for (const fn of Object.keys(selected.functions ?? {}))
      if (!record.suspending.has(`${name}#${fn}`))
        throw new Error(`The prepared adapter does not suspend import ${name}#${fn}`);
  }
}

/** Install one compiled Orb; `release` frees the verifier handle exactly once. */
export async function openComponent(
  record: PreparedRecord,
  contract: PreparedContract,
  installation: LoadOptions,
  options: ComponentOptions,
  release: () => void,
): Promise<OrbDefinition> {
  requireJspi();
  const identity = JSON.parse(contract.description()) as { name: string; apiVersion: number };
  if (identity.name !== record.orb.name || identity.apiVersion !== record.orb.apiVersion)
    throw new Error("The prepared adapter was generated for another Orb identity");
  checkPublicWasi(record.imports);
  const prepared = query<InstallationPlan>(contract, {
    kind: "install",
    name: identity.name,
    apiVersion: identity.apiVersion,
    requires: installation.requires ?? {},
    grants: installation.grants ?? {},
    filesystem: Object.keys(options.providers?.filesystem ?? {}),
    // Network grants resolve only against socket providers and HTTP grants only against HTTP
    // providers, so one registry never satisfies the other kind of grant.
    network: Object.keys(options.providers?.network ?? {}),
    http: Object.keys(options.providers?.http ?? {}),
  });
  const graph = query<Described>(contract, { kind: "describe" });
  checkAdapter(record, graph, prepared);
  const timeout = positive("timeoutMs", options.limits?.timeoutMs ?? 10000);
  const memoryBytes = positive("memoryBytes", options.limits?.memoryBytes ?? 128 << 20);
  const fuel = BigInt(positive("fuel", options.limits?.fuel ?? 100_000_000));
  const maximumPages = Math.floor(memoryBytes / PAGE);
  if (maximumPages < 1 || maximumPages > record.maximumPages)
    throw new Error(
      `limits.memoryBytes must hold between 1 and ${record.maximumPages} pages of 64 KiB`,
    );
  const grants = installation.grants ?? {};
  const system = grants.system ?? [];
  let capabilities: BrowserCapabilities | undefined;
  let stdio: StdioStreams | undefined;
  const port = new ComponentPort(
    timeout,
    async () => {
      const cleanup: Promise<unknown>[] = [Promise.resolve().then(() => capabilities?.dispose())];
      // The revision owns the stdio providers its factory returned and closes them last.
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
    capabilities = await BrowserCapabilities.open(grants, options.providers ?? {});
    if (system.includes("cli"))
      stdio = options.providers?.stdio?.(identity.name, installation.cli ?? {}) ?? {};
    const clocks = system.includes("clocks")
      ? checkedClocks(options.providers?.clocks ?? browserClocks())
      : undefined;
    const random = system.includes("random")
      ? revisionRandom(options.providers?.random ?? browserRandom())
      : undefined;
    // Each socket interface uses exactly the provider its import binding selected.
    const bound = (name: string) => {
      const entry = graph.imports.find((item) => item.contract === name);
      const binding = entry && prepared.bindings[entry.binding];
      return binding?.kind === "host" ? binding.provider : undefined;
    };
    const convert = converters(graph);
    const revision = new ComponentRevision(port, contract, graph, timeout);
    const network = capabilities.network;
    const imports: Record<string, unknown> = publicWasi(record.imports, {
      clocks,
      random,
      ...(stdio ? { cli: { options: installation.cli ?? {}, stdio } } : {}),
      mounts: () => capabilities?.mounts ?? [],
      host: port.wasi,
      http: network,
      ingress: (bridge) => (revision.ingressBridge = bridge as never),
      sockets: {
        types: network.sockets(bound("wasi:sockets/types@0.3.0")),
        lookup: network.sockets(bound("wasi:sockets/ip-name-lookup@0.3.0")),
      },
    });
    for (const name of record.imports) {
      if (Object.hasOwn(imports, name)) continue;
      const selected = graph.imports.find((item) => item.binding === name);
      if (selected?.type_only) {
        imports[name] = {};
        continue;
      }
      // `checkAdapter` admitted only Orb-bound dependency imports here.
      if (!selected) throw new Error(`Browser component execution cannot provide import ${name}`);
      const view = graph.references[name];
      imports[name] = view
        ? referenceImport(port, name, referenceView(graph, name, view, convert))
        : resourceImport(
            port,
            name,
            Object.entries(selected.functions ?? {}).map(([method, fn]) => {
              const roles = resourceRoles(graph, selected.contract ?? name, {
                ...fn,
                name: method,
              } as GraphFunction);
              return {
                name: method,
                spelling: graph.javascriptNames[method]!,
                roles,
                values: convert(resourceDataFunction(fn as GraphFunction, roles)),
              };
            }),
          );
    }
    // Pages are shared by all core modules. Instantiation has finite fuel; subsequent fuel
    // resets occur only between groups of physically overlapping callbacks.
    const limits: CoreLimits = {
      fuel: new WebAssembly.Global({ value: "i64", mutable: true }, fuel),
      pages: new WebAssembly.Global({ value: "i64", mutable: true }, BigInt(maximumPages)),
    };
    port.limits = limits;
    const instance = await instantiate(record, imports, port.observer, limits);
    // Fail closed when the glue never reported its task accessor: it is not instrumented.
    if (!port.attached) throw new Error("The prepared component glue is not instrumented");
    port.exports = instance as never;
    const description = compiledDescription(
      graph,
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
