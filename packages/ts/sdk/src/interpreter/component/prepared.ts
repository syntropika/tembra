/**
 * Application-owned prepared components: the only way a compiled Orb runs in a browser.
 *
 * Tembra's build tooling verifies a compiled artifact's real WIT types natively, generates its
 * bindings once with JCO, instruments the glue and every core module, and emits an ES module that
 * calls `preparedComponent`. That module is host-trusted application code: the application
 * imports it explicitly and registers it with `createHost({ isolated: { components } })`.
 * Nothing an Orb names (a URL, a sidecar, a claimed hash) is ever imported or trusted; an
 * uploaded artifact selects an adapter only when the browser verifier hashed its actual bytes
 * to the adapter's `artifact` digest.
 *
 * This module is small and loaded with the browser facade; execution code loads lazily.
 */

/** Format of the generated adapter modules this SDK accepts. */
export const PREPARED_FORMAT = 1;
/** The generator whose output the build-time glue instrumentation recognizes exactly. */
export const PREPARED_GENERATOR = "@bytecodealliance/jco-transpile@0.23.0";
/** Pages of a 32-bit memory: the build-time clamp; installations budget below it at runtime. */
export const PREPARED_MAXIMUM_PAGES = 65536;
/** Generated component core modules; all are instrumented with `tembra:limits`. */
export const COMPONENT_CORE = /^orb\.core[0-9]*\.wasm$/;
/** JCO's own JSPI trampolines: fixed helpers carrying no component code. */
export const TRAMPOLINE_CORE =
  /^orb\.conditional-(?:host-import-[0-9]+|suspending-(?:[123]-i32-to-i32|3-i32-to-void))\.wasm$/;

/** The generated `instantiate` export of instrumented JCO glue. */
export type Instantiate = (
  getCoreModule: (name: string) => Promise<WebAssembly.Module>,
  imports: Record<string, unknown>,
  instantiateCore: (
    module: WebAssembly.Module,
    imports: WebAssembly.Imports,
  ) => Promise<WebAssembly.Instance>,
  options: { tembraObserver: unknown },
) => Promise<Record<string, unknown>>;

/** Core module bytes as a generated loader supplies them. */
export type CoreSource = Response | ArrayBuffer | ArrayBufferView | WebAssembly.Module;

/** One generated core module of a prepared component. */
export interface PreparedCoreSource {
  /** The name the generated glue requests. */
  readonly name: string;
  /** Pages one instantiation allocates (instrumented component cores only). */
  readonly pages?: number;
  /** Load the module's bytes; called at most once per successful compilation. */
  readonly load: () => CoreSource | Promise<CoreSource>;
}

/** What a generated adapter module passes to `preparedComponent`. */
export interface PreparedComponentSource {
  readonly format: number;
  readonly generator: string;
  /** Identity of the prepared Orb, checked against the artifact's own metadata. */
  readonly orb: { readonly name: string; readonly apiVersion: number };
  /** Lowercase hexadecimal SHA-256 of the exact original artifact bytes. */
  readonly artifact: string;
  /** The page clamp the core modules were instrumented with. */
  readonly maximumPages: number;
  /** Exact versioned top-level import names of the verified component. */
  readonly imports: readonly string[];
  /** `import#function` names generated as suspending (JSPI) host imports. */
  readonly suspending: readonly string[];
  /** `export#function` names generated as promising (JSPI) exports. */
  readonly promising: readonly string[];
  readonly cores: readonly PreparedCoreSource[];
  readonly instantiate: Instantiate;
}

/**
 * An application-owned compiled component adapter, created by a build-generated module. Pass it
 * to `createHost({ isolated: { components: [...] } })`; its fields are informational.
 */
export interface PreparedComponent {
  readonly orb: { readonly name: string; readonly apiVersion: number };
  /** SHA-256 of the exact compiled artifact this adapter executes. */
  readonly artifact: string;
}

/** The checked, immutable adapter record behind one `PreparedComponent`. */
export interface PreparedRecord {
  readonly orb: PreparedComponent["orb"];
  readonly artifact: string;
  readonly maximumPages: number;
  readonly imports: readonly string[];
  readonly suspending: ReadonlySet<string>;
  readonly promising: ReadonlySet<string>;
  readonly cores: readonly PreparedCoreSource[];
  readonly instantiate: Instantiate;
}

/** A browser platform requirement is missing; no other engine is tried. */
export class BrowserPlatformError extends Error {
  override readonly name = "BrowserPlatformError";
}

const records = new WeakMap<object, PreparedRecord>();

const invalid = (detail: string) =>
  new TypeError(`Invalid prepared component module (${detail}); rebuild it with Tembra tooling`);

function names(value: unknown, field: string): readonly string[] {
  if (!Array.isArray(value) || value.some((name) => typeof name !== "string"))
    throw invalid(`${field} must list names`);
  if (new Set(value).size !== value.length) throw invalid(`duplicate ${field} name`);
  return Object.freeze([...value]);
}

/**
 * Check a build-generated adapter description and capture it. Only generated modules call this;
 * applications register the returned value.
 */
export function preparedComponent(source: PreparedComponentSource): PreparedComponent {
  if (source?.format !== PREPARED_FORMAT) throw invalid("unsupported format");
  if (source.generator !== PREPARED_GENERATOR)
    throw invalid(`generated by ${String(source.generator)}, expected ${PREPARED_GENERATOR}`);
  const orb = source.orb;
  if (
    typeof orb?.name !== "string" ||
    !orb.name ||
    !Number.isSafeInteger(orb.apiVersion) ||
    orb.apiVersion <= 0
  )
    throw invalid("orb identity");
  if (typeof source.artifact !== "string" || !/^[0-9a-f]{64}$/.test(source.artifact))
    throw invalid("artifact digest");
  if (
    !Number.isSafeInteger(source.maximumPages) ||
    source.maximumPages < 1 ||
    source.maximumPages > PREPARED_MAXIMUM_PAGES
  )
    throw invalid("maximum pages");
  if (typeof source.instantiate !== "function") throw invalid("instantiate");
  if (!Array.isArray(source.cores)) throw invalid("cores");
  const cores = source.cores.map((core): PreparedCoreSource => {
    if (typeof core?.name !== "string" || typeof core.load !== "function")
      throw invalid("core module");
    if (COMPONENT_CORE.test(core.name)) {
      if (!Number.isSafeInteger(core.pages) || core.pages! < 0 || core.pages! > source.maximumPages)
        throw invalid(`initial pages of ${core.name}`);
      return Object.freeze({ name: core.name, pages: core.pages!, load: core.load });
    }
    if (TRAMPOLINE_CORE.test(core.name) && core.pages === undefined)
      return Object.freeze({ name: core.name, load: core.load });
    throw invalid(`unexpected core module ${core.name}`);
  });
  if (new Set(cores.map((core) => core.name)).size !== cores.length)
    throw invalid("duplicate core module");
  if (!cores.some((core) => core.pages !== undefined)) throw invalid("no component core module");
  const identity = Object.freeze({ name: orb.name, apiVersion: orb.apiVersion });
  const record: PreparedRecord = {
    orb: identity,
    artifact: source.artifact,
    maximumPages: source.maximumPages,
    imports: names(source.imports, "import"),
    suspending: new Set(names(source.suspending, "suspending import")),
    promising: new Set(names(source.promising, "promising export")),
    cores: Object.freeze(cores),
    instantiate: source.instantiate,
  };
  const component: PreparedComponent = Object.freeze({
    orb: identity,
    artifact: source.artifact,
  });
  records.set(component, record);
  return component;
}

/**
 * Index registered adapters by artifact digest. A value not created by `preparedComponent` (for
 * example a copied object or a different SDK instance) is refused rather than trusted.
 */
export function preparedRegistry(
  components: readonly PreparedComponent[] | undefined,
): ReadonlyMap<string, PreparedRecord> {
  const registry = new Map<string, PreparedRecord>();
  if (components === undefined) return registry;
  if (!Array.isArray(components))
    throw new TypeError("isolated.components must list prepared components");
  for (const component of components) {
    const record = records.get(component as object);
    if (!record)
      throw new TypeError(
        "isolated.components accepts only modules generated by Tembra tooling (build with " +
          "`browser: true`) and imported from the same @syntropika/tembra-sdk",
      );
    const existing = registry.get(record.artifact);
    if (existing && existing !== record)
      throw new TypeError(`Two prepared components are registered for artifact ${record.artifact}`);
    registry.set(record.artifact, record);
  }
  return registry;
}
