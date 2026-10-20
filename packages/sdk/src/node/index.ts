import { randomUUID } from "node:crypto";
import { memoryUsage } from "node:process";
import { getHeapStatistics } from "node:v8";

import type { ContextHandle, ReferenceOf } from "../binding.js";
import type { AnyContract, InputOf } from "../contracts.js";
import type {
  AcquisitionOptions,
  FacetDefinition,
  ManagedContext,
  Registry,
  ReloadOptions,
  ReloadReport,
} from "../executor.js";
import { executorFailure } from "../host/error.js";
import { bind, setupShim, signals, wrapContext, wrapRegistry } from "../host/executor.js";
import type { RawContext, RawReference, RawRegistryView } from "../host/executor.js";
import { facetInputs, readyReference } from "../host/index.js";
import type { DynamicFacet, LoadableFacet, FacetInput, TembraHost } from "../host/index.js";
import {
  registerManagerBridge,
  closeManagedHosts,
  disposeManagerLinks,
} from "../manager/bridge.js";
import { canonicalJson } from "../portable.js";
import { nativeTransport } from "./bridge.js";
import type { NativeHostDiagnostics, NodeHostDiagnostics } from "./diagnostics.js";
import { nodeEsmEvaluator } from "./esm.js";
import type { Preparation, PreparedFacet } from "./loader.js";
import { nodeModuleBase } from "./source.js";

export { createHostManager, forkDriver } from "./manager.js";
export type {
  HostManager,
  HostInstance,
  HostDriver,
  HostManagerOptions,
  ManagedChannel,
  ManagedMessage,
  ForkDriverOptions,
} from "./manager.js";

export type {
  NodeHostDiagnostics,
  NodeLoaderDiagnostics,
  NodeGenerationDiagnostics,
} from "./diagnostics.js";

export type CreateNodeHostOptions = {
  readonly name?: string;
  /** Directory used for relative source paths. Defaults to the working directory at Host creation. */
  readonly moduleBase?: string | URL;
  /** Explicit addon path for development or an application-provided binary distribution. */
  readonly binary?: string | URL;
};
export type NodeHost = Omit<TembraHost, "diagnostics"> & {
  readonly diagnostics: () => NodeHostDiagnostics;
};

/** Creates a real native Rust Host and its local scheduler through Node-API. */
// oxlint-disable-next-line typescript/require-await -- The cross-platform factory has an asynchronous contract; Node-API construction is synchronous.
export async function createHost(options: CreateNodeHostOptions = {}): Promise<NodeHost> {
  if (options.name === "") {
    throw executorFailure("host_name", "host name is empty");
  }
  const moduleBase = nodeModuleBase(options.moduleBase);
  const transport = nativeTransport(options.name ?? "node", options.binary);
  const evaluator = nodeEsmEvaluator(transport.authority);
  let closing: Promise<void> | undefined;
  let closedDiagnostics: NodeHostDiagnostics | undefined;
  const preparations = new Set<Promise<unknown>>();
  const check = () => {
    if (closing) {
      throw new Error("Host is closed");
    }
  };
  async function prepare<T>(
    inputs: readonly FacetInput[],
    commit: (facets: readonly PreparedFacet[]) => Promise<T>,
  ): Promise<T> {
    check();
    const admission = transport.request<Preparation>("beginPreparation");
    const pending = (async () => {
      try {
        const { withPreparedFacets } = await import("./loader.js");
        return await withPreparedFacets(
          transport,
          evaluator,
          inputs,
          moduleBase,
          admission,
          check,
          commit,
        );
      } finally {
        admission.free();
      }
    })();
    preparations.add(pending);
    // Both observers fulfill; abandoning a public result never abandons physical preparation.
    void pending.then(
      () => {
        preparations.delete(pending);
      },
      () => {
        preparations.delete(pending);
      },
    );
    return await pending;
  }
  function snapshotDiagnostics(): NodeHostDiagnostics {
    const native = transport.request<NativeHostDiagnostics>("diagnostics");
    return Object.freeze({
      ...native,
      loader: Object.freeze({
        ...native.loader,
        source: Object.freeze({
          ...native.loader.source,
          evaluator: "jiti",
          compilerRealm: "isolated" as const,
        }),
        executionProfile: evaluator.profile,
        evaluator: evaluator.name,
      }),
      bridge: transport.counts(),
      memory: Object.freeze({
        ...memoryUsage(),
        heapSizeLimit: getHeapStatistics().heap_size_limit,
      }),
    });
  }
  const host: NodeHost = {
    registerHook(definition) {
      check();
      transport.request("registerHook", [canonicalJson(definition.descriptor)]);
    },
    registerOpaque(schema) {
      check();
      transport.request("registerOpaque", [schema.native]);
    },
    async install(facet: FacetDefinition) {
      check();
      await transport.request("install", [
        facet.name,
        canonicalJson(facet.uses.map((token) => token.descriptor)),
        setupShim(facet),
      ]);
    },
    async load(facet: LoadableFacet, selected = "default") {
      check();
      await prepare(
        facetInputs([
          typeof facet === "string" || facet instanceof URL
            ? { module: facet, selected }
            : { artifact: facet, selected },
        ]),
        async (facets) => {
          await transport.request("installPrepared", [facets[0]]);
        },
      );
    },
    async reload(
      facets: readonly DynamicFacet[],
      reloadOptions: ReloadOptions = {},
    ): Promise<ReloadReport> {
      check();
      const { signal, ...flags } = reloadOptions;
      const rawSignal = signal === undefined ? undefined : signals.get(signal);
      if (signal !== undefined && rawSignal === undefined) {
        throw new Error("signal was not issued by this binding");
      }
      const inputs = facetInputs(facets);
      const optionsJson = canonicalJson(flags);
      const report = await prepare(
        inputs,
        async (candidates) =>
          await transport.request<
            Promise<Omit<ReloadReport, "previousVersion"> & { previousVersion: number | null }>
          >("reload", [candidates, optionsJson, rawSignal]),
      );
      return Object.freeze({ ...report, previousVersion: report.previousVersion ?? undefined });
    },
    hold<C extends AnyContract>(
      contract: C,
      input: InputOf<C>,
      acquisition: AcquisitionOptions = {},
    ): ReferenceOf<C> {
      check();
      const raw = transport.request<RawReference>("hold", [
        canonicalJson(contract.descriptor),
        canonicalJson(input),
        canonicalJson(acquisition),
      ]);
      return bind(contract, raw);
    },
    async get<C extends AnyContract>(
      contract: C,
      input: InputOf<C>,
      acquisition?: AcquisitionOptions,
    ): Promise<ReferenceOf<C>> {
      return await readyReference(host.hold(contract, input, acquisition));
    },
    async root<T>(operation: (context: ManagedContext) => T | PromiseLike<T>): Promise<T> {
      check();
      return await transport.request<Promise<T>>("root", [
        (raw: RawContext) => operation(wrapContext(raw)),
      ]);
    },
    registry<T>(definition: Registry<T>) {
      check();
      return wrapRegistry<T>(
        transport.request<RawRegistryView>("registry", [canonicalJson(definition)]),
      );
    },
    cancel(context: ContextHandle) {
      check();
      transport.request("cancel", [context.handle]);
    },
    diagnostics() {
      return closedDiagnostics ?? snapshotDiagnostics();
    },
    async close() {
      closing ??= (async () => {
        try {
          const managerResult = await Promise.allSettled([closeManagedHosts(host)]);
          const coreClose = transport.request<Promise<void>>("close");
          const [result] = await Promise.allSettled([coreClose, ...preparations]);
          if (managerResult[0]?.status === "rejected") {
            throw managerResult[0].reason;
          }
          if (result?.status === "rejected") {
            throw result.reason;
          }
        } finally {
          disposeManagerLinks(host);
          transport.flush();
          closedDiagnostics = snapshotDiagnostics();
          transport.dispose();
        }
      })();
      await closing;
    },
  };
  let rpcIdentity = false;
  registerManagerBridge(host, {
    name: options.name ?? "node",
    attach(peer, child) {
      check();
      if (!rpcIdentity) {
        transport.request("rpcIdentity", [randomUUID().replaceAll("-", "")]);
        rpcIdentity = true;
      }
      return transport.request("rpcAttach", [peer, child]);
    },
  });
  return host;
}
