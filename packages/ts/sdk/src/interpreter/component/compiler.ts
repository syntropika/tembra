/**
 * Tembra's trusted component compiler for browsers.
 *
 * Glue is generated here, from the verified component bytes, by JCO's own bindgen component.
 * Nothing an Orb ships (sidecar JavaScript, a manifest, a claimed hash) is ever evaluated: an
 * attacker controls both the sidecar and the hash it claims. Core modules are compiled only from
 * the generated file map, never fetched. The generated ESM is loaded from a Blob URL that is
 * revoked as soon as the import settles, which requires the application CSP to allow
 * `script-src blob:` and `'wasm-unsafe-eval'`; `'unsafe-eval'` is never needed.
 *
 * Current JCO component-model async output requires JSPI (`WebAssembly.Suspending` and
 * `WebAssembly.promising`) even in its synchronous mode; without it execution is refused rather
 * than falling back to another engine.
 */

import { fixLiftOwnTables, fixResultErrExit, instrumentGlue } from "./instrument.ts";

/**
 * Tembra's packaged bindgen module (browser-only); it locates its two core Wasm assets through
 * `import.meta.url`. The specifier stays a literal so application bundlers can discover it.
 */
const loadCompiler = () => import("@syntropika/tembra-engine-wasm/compiler");

interface GenerateOptions {
  name: string;
  noTypescript: boolean;
  instantiation: { tag: "async" };
  noNodejsCompat: boolean;
  base64Cutoff: number;
  map: [string, string][];
  asyncMode: { tag: "jspi"; val: { imports: string[]; exports: string[] } };
  emitConditionalSuspendingWasm: boolean;
}
interface Bindgen {
  $init: Promise<void>;
  generate(
    component: Uint8Array,
    options: GenerateOptions,
  ): {
    files: [string, Uint8Array][];
    imports: string[];
    exports: [string, string][];
  };
}
type Instantiate = (
  getCoreModule: (name: string) => Promise<WebAssembly.Module>,
  imports: Record<string, unknown>,
  instantiateCore: (
    module: WebAssembly.Module,
    imports: WebAssembly.Imports,
  ) => Promise<WebAssembly.Instance>,
  options: { tembraObserver: TaskObserver },
) => Promise<Record<string, unknown>>;

/** Lifecycle notifications of one instance from the instrumented trusted glue. */
export interface TaskObserver {
  /** Called once at instantiation with an accessor valid only synchronously at host-import entry. */
  attach(access: { currentRootTask(): number | null }): void;
  taskStarted(task: { id: number; entry: string; isAsync: boolean }): void;
  taskExited(task: { id: number; rejected?: boolean; error?: unknown }): void;
  trapped(error: unknown): void;
}

/** A browser platform requirement is missing; no other engine is tried. */
export class BrowserPlatformError extends Error {
  override readonly name = "BrowserPlatformError";
}

/** Refuse before any work when the engine lacks JSPI. */
export function requireJspi(): void {
  const wasm = globalThis.WebAssembly as unknown as Record<string, unknown> | undefined;
  if (typeof wasm?.Suspending !== "function" || typeof wasm?.promising !== "function")
    throw new BrowserPlatformError(
      "Browser Orb execution requires WebAssembly JSPI (WebAssembly.Suspending and " +
        "WebAssembly.promising); this browser does not provide it. Chrome 154 is the qualified browser.",
    );
}

/** What the generator needs to know about one verified component. */
export interface CompileRequest {
  /** Verified component bytes. */
  readonly component: Uint8Array;
  /** Exact versioned top-level import names read from the verified binary. */
  readonly imports: readonly string[];
  /** `interface@version#function` imports whose host implementation suspends (sync WIT, async host). */
  readonly suspendingImports: readonly string[];
  /** `interface@version#function` exports that may suspend on such an import. */
  readonly promisingExports: readonly string[];
  /** The verifier's core instrumenter: fuel at function entries and loops, bounded memory. */
  readonly instrumentCore: (bytes: Uint8Array, maximumPages: number) => InstrumentedCore;
  /** Linear-memory budget of one instance, in 64 KiB pages. */
  readonly maximumPages: number;
}

/** One instrumented core module, owned by the caller until `free`. */
export interface InstrumentedCore {
  bytes(): Uint8Array;
  initialPages(): bigint;
  free(): void;
}

/**
 * The per-instance limits shared by all its core modules: `fuel` is debited at function entries
 * and loop headers (a trap at zero), `pages` by every instantiation and `memory.grow`.
 */
export interface CoreLimits {
  readonly fuel: WebAssembly.Global;
  readonly pages: WebAssembly.Global;
}

/** Generated component core modules; JCO's own suspension trampolines carry no component code. */
const COMPONENT_CORE = /^orb\.core[0-9]*\.wasm$/;

/** Generated bindings for one component; each `instantiate` creates an isolated instance. */
export interface CompiledComponent {
  instantiate(
    imports: Record<string, unknown>,
    observer: TaskObserver,
    limits: CoreLimits,
  ): Promise<Record<string, unknown>>;
}

/** Lazily loaded, shared by every installation of one browser execution backend. */
export class TrustedCompiler {
  #bindgen: Promise<Bindgen> | undefined;

  constructor(private readonly load: () => Promise<unknown> = loadCompiler) {}

  /** Load and initialize the bindgen component once; a failed load may be retried. */
  private bindgen(): Promise<Bindgen> {
    if (!this.#bindgen) {
      const loading = this.load().then(async (module) => {
        const bindgen = module as Bindgen;
        await bindgen.$init;
        return bindgen;
      });
      this.#bindgen = loading;
      loading.catch(() => {
        if (this.#bindgen === loading) this.#bindgen = undefined;
      });
    }
    return this.#bindgen;
  }

  /** Generate glue from the actual component and return an isolated instantiator. */
  async compile(request: CompileRequest): Promise<CompiledComponent> {
    requireJspi();
    const bindgen = await this.bindgen();
    const output = bindgen.generate(request.component, {
      name: "orb",
      noTypescript: true,
      instantiation: { tag: "async" },
      noNodejsCompat: true,
      base64Cutoff: 0,
      // JCO drops import versions by default, which would let a runtime-internal WASI 0.2
      // interface share a key with the contract's WASI 0.3 one. Exact entries keep each apart.
      map: request.imports.map((name) => [name, name]),
      asyncMode: {
        tag: "jspi",
        val: {
          imports: [...request.suspendingImports],
          exports: [...request.promisingExports],
        },
      },
      emitConditionalSuspendingWasm: true,
    });
    const files = new Map(output.files);
    const glue = files.get("orb.js");
    if (!glue) throw new Error("Trusted compiler produced no component glue");
    files.delete("orb.js");
    // Physical task exit and traps are observed through exact, fail-closed instrumentation.
    const instantiate = await importGlue(
      new TextEncoder().encode(
        fixLiftOwnTables(fixResultErrExit(instrumentGlue(new TextDecoder().decode(glue)))),
      ),
    );
    // Core modules compile once per component; instances never share memories or tables.
    // Every component core module is instrumented; anything instantiated through
    // `instantiateCore` without instrumentation is refused.
    const modules = new Map<string, Promise<WebAssembly.Module>>();
    const initial = new Map<WebAssembly.Module, bigint>();
    for (const [name, bytes] of files) {
      if (!name.endsWith(".wasm")) continue;
      if (!COMPONENT_CORE.test(name)) {
        modules.set(name, WebAssembly.compile(bytes as BufferSource));
        continue;
      }
      const core = request.instrumentCore(bytes, request.maximumPages);
      try {
        const module = await WebAssembly.compile(core.bytes() as BufferSource);
        initial.set(module, core.initialPages());
        modules.set(name, Promise.resolve(module));
      } finally {
        core.free();
      }
    }
    const core = (name: string) => {
      const module = modules.get(name);
      if (!module) throw new Error("Unknown generated core module");
      return module;
    };
    /** Debit a module's initial pages at every instantiation, before its start function. */
    const instantiateCore =
      (limits: CoreLimits) =>
      async (
        module: WebAssembly.Module,
        imports: WebAssembly.Imports,
      ): Promise<WebAssembly.Instance> => {
        const pages = initial.get(module);
        if (pages === undefined) throw new Error("Uninstrumented core module");
        if ((limits.pages.value as bigint) < pages)
          throw new WebAssembly.RuntimeError("memory limit exceeded");
        limits.pages.value = (limits.pages.value as bigint) - pages;
        try {
          return await WebAssembly.instantiate(module, {
            ...imports,
            "tembra:limits": limits as never,
          });
        } catch (error) {
          // Refund only when no instance exists.
          limits.pages.value = (limits.pages.value as bigint) + pages;
          throw error;
        }
      };
    return {
      instantiate: (imports, observer, limits) =>
        instantiate(core, imports, instantiateCore(limits), {
          tembraObserver: observer,
        }),
    };
  }
}

/** Import generated ESM from a short-lived Blob URL. */
async function importGlue(glue: Uint8Array): Promise<Instantiate> {
  const url = URL.createObjectURL(new Blob([glue as BlobPart], { type: "text/javascript" }));
  let module: { instantiate?: unknown };
  try {
    module = await import(/* @vite-ignore */ /* webpackIgnore: true */ url);
  } catch (error) {
    throw new BrowserPlatformError(
      "Loading trusted component glue failed. Browser Orb execution requires a Content-Security-" +
        "Policy allowing `script-src blob:` and `'wasm-unsafe-eval'` (never `'unsafe-eval'`).",
      { cause: error },
    );
  } finally {
    URL.revokeObjectURL(url);
  }
  if (typeof module.instantiate !== "function")
    throw new Error("Trusted component glue has no instantiate export");
  return module.instantiate as Instantiate;
}
