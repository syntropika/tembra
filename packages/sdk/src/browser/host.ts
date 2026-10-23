import type { ContextHandle, ReferenceOf } from "../binding.js";
import type { AnyContract, InputOf } from "../contracts.js";
import type {
  AcquisitionOptions,
  FacetDefinition,
  ManagedContext,
  Registry,
  ReloadOptions,
} from "../executor.js";
import { bind, wrapContext, wrapRegistry } from "../host/executor.js";
import type { ExecutorWasmModule } from "../host/executor.js";
import { facetInputs, readyReference } from "../host/index.js";
import type { DynamicFacet, LoadableFacet, TembraHost } from "../host/index.js";
import {
  checkObservationLimit,
  drainFinalObservations,
  parseObservationBatch,
} from "../host/observability.js";
import type { ObservationBatch, ObservationOptions } from "../host/observability.js";
import {
  registerManagerBridge,
  closeManagedHosts,
  disposeManagerLinks,
} from "../manager/bridge.js";
import { canonicalJson } from "../portable.js";
import type { WasmModule } from "./abi.js";
import { browserErrors } from "./diagnostics.js";
import type { BrowserHostDiagnostics } from "./diagnostics.js";
import { BrowserExecutor } from "./executor.js";
import { browserModuleBase, moduleDiagnostics } from "./module.js";

/** Core-owned idle consumer cache policy. */
export type BrowserHostOptions = {
  readonly observability?: ObservationOptions;
  readonly consumerLinger?: {
    readonly types: Readonly<Record<string, number>>;
    readonly capacity?: number;
  };
};
/** A Host-owned websocket connection. Closing the Host also disposes its native wrapper. */
export type BrowserConnection = {
  readonly ready: () => Promise<void>;
  readonly closed: () => Promise<string>;
  readonly close: () => void;
  readonly status: () => string;
};

export type BrowserWasmModule = WasmModule & ExecutorWasmModule;
export type CreateBrowserHostOptions = BrowserHostOptions & {
  readonly name?: string;
  /** Resolves relative module URLs. Defaults to document.baseURI or the Worker location. */
  readonly baseURL?: string | URL;
  /** An initialized module, or a loader that initializes it. Each call creates an independent Host. */
  readonly wasm?: BrowserWasmModule | (() => Promise<BrowserWasmModule>);
  /** Input for the packaged wasm-bindgen initializer, including a custom WASM asset when needed. */
  readonly wasmInput?: unknown;
};

/** Owns raw binding wrappers without changing Rust reference ownership. */
function wrappers() {
  const owned = new Set<WeakRef<{ free: () => void }>>();
  const wrapped = new WeakMap<object, WeakRef<object>>();
  function track<T>(value: T): T {
    if (value instanceof Promise) {
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The generated wasm-bindgen ABI preserves this issued wrapper or submitted callback result type.
      return value.then(track) as T;
    }
    if (
      value === null ||
      typeof value !== "object" ||
      !("__wbg_ptr" in value) ||
      !("free" in value) ||
      typeof value.free !== "function"
    ) {
      return value;
    }
    const known = wrapped.get(value)?.deref();
    if (known) {
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The generated wasm-bindgen ABI preserves this issued wrapper or submitted callback result type.
      return known as T;
    }
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The generated wasm-bindgen ABI preserves this issued wrapper or submitted callback result type.
    const raw = value as T & { free: () => void };
    const weak = new WeakRef(raw);
    owned.add(weak);
    if (owned.size % 64 === 0) {
      for (const item of owned) {
        if (item.deref() === undefined) {
          owned.delete(item);
        }
      }
    }
    const proxy = new Proxy(raw, {
      get(target, key) {
        const member: unknown = Reflect.get(target, key);
        if (typeof member !== "function") {
          return member;
        }
        return (...args: unknown[]) => {
          if (key === "free") {
            owned.delete(weak);
          }
          const callbacks = args.map((arg) =>
            typeof arg === "function"
              ? (...values: unknown[]): unknown => Reflect.apply(arg, undefined, values.map(track))
              : arg,
          );
          const result: unknown = Reflect.apply(member, target, callbacks);
          return track(result);
        };
      },
    });
    wrapped.set(value, new WeakRef(proxy));
    return proxy;
  }
  return {
    track,
    free() {
      for (const weak of [...owned].toReversed()) {
        weak.deref()?.free();
      }
      owned.clear();
    },
  };
}

export type BrowserHost = Omit<TembraHost, "diagnostics"> & {
  readonly diagnostics: () => BrowserHostDiagnostics;
  /** Browser-specific transport; no reconnect policy is installed. */
  connect: (url: string, provider: string) => BrowserConnection;
};

/** Creates and binds the existing BrowserExecutor to exactly one real Rust/WASM Host. */
export async function createHost(options: CreateBrowserHostOptions = {}): Promise<BrowserHost> {
  let module: BrowserWasmModule;
  if (options.wasm === undefined) {
    // The build ships this sibling module and its default relative WASM asset together.
    const url = new URL("../../wasm/tembra_browser_wasm.js", import.meta.url);
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The generated wasm-bindgen ABI preserves this issued wrapper or submitted callback result type.
    const loaded = (await import(url.href)) as BrowserWasmModule & {
      default: (input?: unknown) => Promise<unknown>;
    };
    await loaded.default(options.wasmInput);
    module = loaded;
  } else {
    module = typeof options.wasm === "function" ? await options.wasm() : options.wasm;
  }
  const owned = wrappers();
  const policy = {
    ...(options.consumerLinger === undefined ? {} : { consumerLinger: options.consumerLinger }),
    ...(options.observability === undefined ? {} : { observability: options.observability }),
  };
  const rawHost = owned.track(
    module.WasmHost.withOptions(options.name ?? "browser", canonicalJson(policy)),
  );
  try {
    const executor = new BrowserExecutor(rawHost, {
      createCheckedWasmExecutor: (host) => owned.track(module.createCheckedWasmExecutor(host)),
    });
    const { runRoot } = executor.rust;
    const { registry } = executor.rust;
    const { hold } = executor.rust;
    let closing: Promise<void> | undefined;
    let closed = false;
    let finalDiagnostics: BrowserHostDiagnostics | undefined;
    let finalObservations: ObservationBatch | null | undefined;
    const snapshotDiagnostics = (): BrowserHostDiagnostics =>
      Object.freeze({
        ...executor.diagnostics(),
        modules: moduleDiagnostics(),
        errors: browserErrors(rawHost.errorsJson()),
      });
    const baseURL = browserModuleBase(options.baseURL);
    const preparations = new Set<Promise<unknown>>();
    const check = () => {
      if (closed || closing) {
        throw new Error("Host is closed");
      }
    };
    const prepare = async <T>(operation: () => Promise<T>): Promise<T> => {
      check();
      if (preparations.size !== 0) {
        throw new Error("another facet preparation is in flight");
      }
      const pending = operation();
      preparations.add(pending);
      try {
        return await pending;
      } finally {
        preparations.delete(pending);
      }
    };
    const host: BrowserHost = {
      registerHook(definition) {
        check();
        executor.rust.registerHook(canonicalJson(definition.descriptor));
      },
      registerOpaque(schema) {
        check();
        executor.rust.registerOpaque(schema.native);
      },
      async install(facet: FacetDefinition) {
        check();
        await executor.install(facet);
      },
      async load(facet: LoadableFacet, selected = "default") {
        check();
        const [input] = facetInputs([
          typeof facet === "string" || facet instanceof URL
            ? { module: facet, selected }
            : { artifact: facet, selected },
        ]);
        if (input === undefined) {
          throw new Error("facet input missing");
        }
        await prepare(async () => {
          await executor.loadInput(input, baseURL, check);
        });
      },
      async reload(facets: readonly DynamicFacet[], reloadOptions?: ReloadOptions) {
        check();
        const inputs = facetInputs(facets);
        return await prepare(
          async () => await executor.reloadInputs(inputs, baseURL, check, reloadOptions),
        );
      },
      hold<C extends AnyContract>(
        contract: C,
        input: InputOf<C>,
        acquisition: AcquisitionOptions = {},
      ): ReferenceOf<C> {
        check();
        // Use the executor's local reference adapter for the same checked contract semantics.
        const raw = hold.call(
          executor.rust,
          canonicalJson(contract.descriptor),
          canonicalJson(input),
          canonicalJson(acquisition),
        );
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
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The generated wasm-bindgen ABI preserves this issued wrapper or submitted callback result type.
        return (await runRoot.call(executor.rust, (raw) => operation(wrapContext(raw)))) as T;
      },
      registry<T>(definition: Registry<T>) {
        check();
        return wrapRegistry<T>(registry.call(executor.rust, canonicalJson(definition)));
      },
      cancel(context: ContextHandle) {
        check();
        rawHost.cancelContext(context.handle);
      },
      observations(limit: number): ObservationBatch | null {
        checkObservationLimit(limit);
        if (finalObservations !== undefined) {
          const { result, remaining } = drainFinalObservations(finalObservations, limit);
          finalObservations = remaining;
          return result;
        }
        return parseObservationBatch(rawHost.observationsJson(limit));
      },
      diagnostics() {
        return finalDiagnostics ?? snapshotDiagnostics();
      },
      connect(url: string, provider: string) {
        check();
        const connection = rawHost.connect(url, provider);
        return Object.freeze({
          ready: async () => {
            await connection.ready();
          },
          closed: async () => await connection.closed(),
          close: () => {
            connection.close();
          },
          status: () => connection.status(),
        });
      },
      async close() {
        closing ??= (async () => {
          try {
            const managerResult = await Promise.allSettled([closeManagedHosts(host)]);
            const results = await Promise.allSettled([rawHost.close(), ...preparations]);
            if (managerResult[0]?.status === "rejected") {
              throw managerResult[0].reason;
            }
            if (results[0]?.status === "rejected") {
              throw results[0].reason;
            }
          } finally {
            disposeManagerLinks(host);
            try {
              finalObservations = parseObservationBatch(rawHost.observationsJson(1_048_576));
              finalDiagnostics = snapshotDiagnostics();
            } finally {
              closed = true;
              owned.free();
            }
          }
        })();
        await closing;
      },
    };
    let rpcIdentity = false;
    registerManagerBridge(host, {
      name: options.name ?? "browser",
      attach(peer, child) {
        check();
        const { rpcIdentity: identity, rpcAttach } = rawHost;
        if (!rpcIdentity) {
          identity.call(rawHost, crypto.randomUUID().replaceAll("-", ""));
          rpcIdentity = true;
        }
        return rpcAttach.call(rawHost, peer, child);
      },
    });
    return host;
  } catch (error) {
    try {
      await rawHost.close();
    } finally {
      owned.free();
    }
    throw error;
  }
}
