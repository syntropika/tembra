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
import {
  componentLayout,
  contentHash as hash,
  fileTemplate,
  instantiateFiles,
  type FileTemplate,
} from "./layout.ts";

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
const canHash = () => typeof globalThis.crypto?.subtle?.digest === "function";

/** Generated bindings for one component; each `instantiate` creates an isolated instance. */
export interface CompiledComponent {
  instantiate(
    imports: Record<string, unknown>,
    observer: TaskObserver,
    limits: CoreLimits,
  ): Promise<Record<string, unknown>>;
}

type CompiledCore = { module: WebAssembly.Module; pages: bigint };
type CachedCore = { module: WeakRef<WebAssembly.Module>; pages: bigint } | Promise<CompiledCore>;

/** Share immutable runtime code; each instantiation still supplies its own memory and limits. */
export class CoreModules {
  readonly #policies = new WeakMap<CompileRequest["instrumentCore"], Map<string, CachedCore>>();

  /** Hash captured bytes, never an artifact's claimed identity or generated file name. */
  async compile(
    bytes: Uint8Array,
    maximumPages: number,
    instrument: CompileRequest["instrumentCore"],
  ): Promise<CompiledCore> {
    const create = async (input: Uint8Array): Promise<CompiledCore> => {
      const core = instrument(input, maximumPages);
      try {
        const pages = core.initialPages();
        return { module: await WebAssembly.compile(core.bytes() as BufferSource), pages };
      } finally {
        core.free();
      }
    };
    // Small adapters are cheap; keep hashing and cache bookkeeping off their path.
    if (bytes.byteLength < 64 << 10 || !canHash()) return create(bytes);
    const captured = bytes.slice();
    const key = `${maximumPages}:${await hash(captured)}`;
    let entries = this.#policies.get(instrument);
    if (!entries) {
      entries = new Map();
      this.#policies.set(instrument, entries);
    }
    const existing = entries.get(key);
    if (existing instanceof Promise) return existing;
    if (existing) {
      const module = existing.module.deref();
      if (module) {
        entries.delete(key);
        entries.set(key, existing);
        return { module, pages: existing.pages };
      }
    }
    const loading = create(captured);
    entries.set(key, loading);
    // The index is bounded and holds completed code weakly, so retired Orbs can be collected.
    while (entries.size > 64) entries.delete(entries.keys().next().value!);
    try {
      const compiled = await loading;
      if (entries.get(key) === loading)
        entries.set(key, { module: new WeakRef(compiled.module), pages: compiled.pages });
      return compiled;
    } catch (error) {
      if (entries.get(key) === loading) entries.delete(key);
      throw error;
    }
  }
}

/** Lazily loaded, shared by every installation of one browser execution backend. */
export class TrustedCompiler {
  #bindgen: Promise<Bindgen> | undefined;
  readonly #modules = new CoreModules();
  readonly #glue = new Map<string, Promise<Instantiate>>();
  readonly #templates = new Map<string, Promise<FileTemplate | undefined>>();

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

  /** JCO keeps instance state inside this factory; equal glue can share its executable code. */
  private async glue(bytes: Uint8Array): Promise<Instantiate> {
    if (!canHash()) return importGlue(bytes);
    const key = await hash(bytes);
    const existing = this.#glue.get(key);
    if (existing) return existing;
    const loading = importGlue(bytes);
    this.#glue.set(key, loading);
    while (this.#glue.size > 32) this.#glue.delete(this.#glue.keys().next().value!);
    try {
      return await loading;
    } catch (error) {
      if (this.#glue.get(key) === loading) this.#glue.delete(key);
      throw error;
    }
  }

  /** Generate glue from the actual component and return an isolated instantiator. */
  async compile(request: CompileRequest): Promise<CompiledComponent> {
    requireJspi();
    const bindgen = await this.bindgen();
    const options: GenerateOptions = {
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
    };
    const files = await this.files(bindgen, request.component, options);
    const glue = files.get("orb.js");
    if (!glue) throw new Error("Trusted compiler produced no component glue");
    files.delete("orb.js");
    // Physical task exit and traps are observed through exact, fail-closed instrumentation.
    const instantiate = await this.glue(glue);
    // Identical runtime modules share code across components; memories and tables stay local.
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
      const compiled = await this.#modules.compile(
        bytes,
        request.maximumPages,
        request.instrumentCore,
      );
      initial.set(compiled.module, compiled.pages);
      modules.set(name, Promise.resolve(compiled.module));
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

  /** Core data is opaque to JCO; reuse its binding template while retaining actual Orb modules. */
  private async files(
    bindgen: Bindgen,
    component: Uint8Array,
    options: GenerateOptions,
  ): Promise<Map<string, Uint8Array>> {
    if (!canHash() || !options.instantiation || options.base64Cutoff !== 0)
      return new Map(bindgen.generate(component, options).files);
    const layout = componentLayout(component);
    if (!layout) return new Map(bindgen.generate(component, options).files);
    const key = `${await hash(layout.identity)}:${JSON.stringify(options)}`;
    const previous = this.#templates.get(key);
    if (previous) {
      const template = await previous;
      if (template) return instantiateFiles(template, layout.modules);
    }
    const output = bindgen.generate(layout.source, options);
    const loading = fileTemplate(output.files, layout.modules);
    this.#templates.set(key, loading);
    while (this.#templates.size > 16) this.#templates.delete(this.#templates.keys().next().value!);
    try {
      await loading;
    } catch (error) {
      if (this.#templates.get(key) === loading) this.#templates.delete(key);
      throw error;
    }
    return new Map(output.files);
  }
}

/** Import generated ESM from a short-lived Blob URL. */
async function importGlue(glue: Uint8Array): Promise<Instantiate> {
  const source = fixLiftOwnTables(fixResultErrExit(instrumentGlue(new TextDecoder().decode(glue))));
  const url = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
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
