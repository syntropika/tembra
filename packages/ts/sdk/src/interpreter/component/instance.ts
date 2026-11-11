/**
 * Instantiation of a prepared component: no generator, parser or encoder runs here.
 *
 * Compiled `WebAssembly.Module`s are shared by every installation of one adapter. Each
 * installation still runs the generated factory once, so JCO state, memories, tables, resource
 * tables and every host import object are its own. Component core modules were instrumented at
 * build time; this module only checks that they import the private limit globals and debits each
 * instantiation's initial pages from the installation's shared budget before it starts.
 */
import { BrowserPlatformError, type PreparedRecord } from "./prepared.ts";

/** Lifecycle notifications of one instance from the instrumented trusted glue. */
export interface TaskObserver {
  /** Called once at instantiation with an accessor valid only synchronously at host-import entry. */
  attach(access: { currentRootTask(): number | null }): void;
  taskStarted(task: { id: number; entry: string; isAsync: boolean }): void;
  taskExited(task: { id: number; rejected?: boolean; error?: unknown }): void;
  trapped(error: unknown): void;
}

/**
 * The per-instance limits shared by all its core modules: `fuel` is debited at function entries
 * and loop headers (a trap at zero), `pages` by every instantiation and `memory.grow`.
 */
export interface CoreLimits {
  readonly fuel: WebAssembly.Global;
  readonly pages: WebAssembly.Global;
}

/** Refuse before any work when the engine lacks JSPI, which the generated glue requires. */
export function requireJspi(): void {
  const wasm = globalThis.WebAssembly as unknown as Record<string, unknown> | undefined;
  if (typeof wasm?.Suspending !== "function" || typeof wasm?.promising !== "function")
    throw new BrowserPlatformError(
      "Compiled Orb execution in browsers requires WebAssembly JSPI (WebAssembly.Suspending and " +
        "WebAssembly.promising); this engine does not provide it.",
    );
}

const LIMITS_MODULE = "tembra:limits";

/** Compiled core modules of one adapter; `pages` is absent for JCO's own trampolines. */
type Cores = ReadonlyMap<string, { readonly module: WebAssembly.Module; readonly pages?: bigint }>;

const compiled = new WeakMap<PreparedRecord, Promise<Cores>>();

async function compile(source: Awaited<ReturnType<PreparedRecord["cores"][number]["load"]>>) {
  if (source instanceof WebAssembly.Module) return source;
  if (typeof Response !== "undefined" && source instanceof Response) {
    if (!source.ok) throw new Error(`Prepared core module request failed: ${source.status}`);
    if (
      typeof WebAssembly.compileStreaming === "function" &&
      source.headers.get("content-type")?.split(";")[0]?.trim() === "application/wasm"
    )
      return WebAssembly.compileStreaming(source);
    return WebAssembly.compile(await source.arrayBuffer());
  }
  return WebAssembly.compile(source as BufferSource);
}

/** Whether a component core imports exactly the instrumented limit globals. */
function instrumented(module: WebAssembly.Module): boolean {
  const limits = WebAssembly.Module.imports(module).filter(
    (entry) => entry.module === LIMITS_MODULE,
  );
  return (
    limits.length === 2 &&
    limits.every((entry) => entry.kind === "global") &&
    limits.some((entry) => entry.name === "fuel") &&
    limits.some((entry) => entry.name === "pages")
  );
}

/** Compile an adapter's core modules once; a failed load may be retried by a later Orb. */
function cores(record: PreparedRecord): Promise<Cores> {
  let loading = compiled.get(record);
  if (!loading) {
    loading = Promise.all(
      record.cores.map(async (core) => {
        const module = await compile(await core.load());
        if (core.pages !== undefined && !instrumented(module))
          throw new Error(`Prepared core module ${core.name} is not instrumented`);
        return [
          core.name,
          core.pages === undefined ? { module } : { module, pages: BigInt(core.pages) },
        ] as const;
      }),
    ).then((entries) => new Map(entries));
    compiled.set(record, loading);
    const current = loading;
    current.catch(() => {
      if (compiled.get(record) === current) compiled.delete(record);
    });
  }
  return loading;
}

/** Instantiate one isolated instance of a prepared component. */
export async function instantiate(
  record: PreparedRecord,
  imports: Record<string, unknown>,
  observer: TaskObserver,
  limits: CoreLimits,
): Promise<Record<string, unknown>> {
  const modules = await cores(record);
  const initial = new Map<WebAssembly.Module, bigint>();
  for (const { module, pages } of modules.values())
    if (pages !== undefined) initial.set(module, pages);
  const getCoreModule = async (name: string) => {
    const core = modules.get(name);
    if (!core) throw new Error("Unknown prepared core module");
    return core.module;
  };
  /** Debit a module's initial pages at every instantiation, before its start function. */
  const instantiateCore = async (
    module: WebAssembly.Module,
    coreImports: WebAssembly.Imports,
  ): Promise<WebAssembly.Instance> => {
    const pages = initial.get(module);
    if (pages === undefined) throw new Error("Uninstrumented core module");
    if ((limits.pages.value as bigint) < pages)
      throw new WebAssembly.RuntimeError("memory limit exceeded");
    limits.pages.value = (limits.pages.value as bigint) - pages;
    try {
      return await WebAssembly.instantiate(module, {
        ...coreImports,
        [LIMITS_MODULE]: limits as never,
      });
    } catch (error) {
      // Refund only when no instance exists.
      limits.pages.value = (limits.pages.value as bigint) + pages;
      throw error;
    }
  };
  return record.instantiate(getCoreModule, imports, instantiateCore, {
    tembraObserver: observer,
  });
}
