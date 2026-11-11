/**
 * Build-time browser preparation of a compiled Orb.
 *
 * The native verifier first checks the component's real WIT types against its contract. JCO then
 * generates bindings once; Tembra instruments the glue (task, exit and trap observation) and every
 * component core module (fuel and page limits), and writes an ES module that the application
 * imports and registers explicitly. The module binds itself to the SHA-256 of the exact artifact.
 * Browsers receive no generator, parser or encoder.
 */
import { createRequire } from "node:module";
import { mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import path from "node:path";
import { query } from "../interpreter/types.ts";
import { HTTP_HANDLER } from "../interpreter/http-ingress.ts";
import {
  COMPONENT_CORE,
  PREPARED_FORMAT,
  PREPARED_GENERATOR,
  PREPARED_MAXIMUM_PAGES,
  TRAMPOLINE_CORE,
  type PreparedComponentSource,
} from "../interpreter/component/prepared.ts";
import type { Described } from "../interpreter/component/revision.ts";
import { converters, type GraphFunction } from "../interpreter/component/values.ts";
import { referenceView } from "../interpreter/component/host.ts";
import { BROWSER_WASI } from "../interpreter/component/wasi.ts";
import { JCO_VERSION, prepareGlue } from "./component-glue.ts";

/** Core modules up to this size are inlined into the adapter module instead of fetched. */
const INLINE_BYTES = 4096;

/** Files written for one prepared adapter. */
export interface BrowserAdapter {
  /** The ES module the application imports and passes to `createHost`. */
  readonly module: string;
  readonly files: readonly string[];
}

/** Refuse a JCO release whose output the glue instrumentation does not recognize exactly. */
async function generator() {
  const entry = createRequire(import.meta.url).resolve("@bytecodealliance/jco-transpile/component");
  const manifest = JSON.parse(
    await readFile(path.join(path.dirname(entry), "..", "package.json"), "utf8"),
  ) as { name?: string; version?: string };
  if (
    manifest.name !== "@bytecodealliance/jco-transpile" ||
    manifest.version !== JCO_VERSION ||
    PREPARED_GENERATOR !== `${manifest.name}@${manifest.version}`
  )
    throw new Error(`Browser preparation requires @bytecodealliance/jco-transpile ${JCO_VERSION}`);
  const bindgen = await import("@bytecodealliance/jco-transpile/component");
  await bindgen.$init;
  return bindgen;
}

/** Check what browser execution provides before generating anything. */
function plan(graph: Described, imports: readonly string[]) {
  const convert = converters(graph);
  const check = (label: string, fn: unknown) => {
    try {
      convert(fn as GraphFunction);
    } catch (error) {
      throw new Error(`Browser preparation cannot execute ${label}: ${(error as Error).message}`);
    }
  };
  const suspending: string[] = [];
  for (const name of imports) {
    if (name.startsWith("wasi:")) {
      if (!BROWSER_WASI.has(name))
        throw new Error(`Browser hosts do not provide ${name}; this Orb cannot run in browsers`);
      continue;
    }
    const selected = graph.imports.find((item) => item.binding === name);
    if (!selected) throw new Error(`The component imports ${name}, which its contract lacks`);
    if (selected.type_only) continue;
    const view = graph.references[name];
    if (view) {
      // A checked reference view: only its acquisition and method receivers are handles, both
      // host-local; every other position is data. No resource crosses between Orbs.
      try {
        referenceView(graph, name, view, convert);
      } catch (error) {
        throw new Error(`Browser preparation cannot execute ${name}: ${(error as Error).message}`);
      }
      for (const fn of Object.keys(selected.functions ?? {})) suspending.push(`${name}#${fn}`);
      continue;
    }
    // Orb dependency calls are asynchronous host work behind a synchronous WIT import.
    for (const [fn, signature] of Object.entries(selected.functions ?? {})) {
      check(`${name}#${fn}`, signature);
      suspending.push(`${name}#${fn}`);
    }
  }
  const promising: string[] = [];
  for (const entry of graph.exports)
    for (const [fn, signature] of Object.entries(entry.functions ?? {})) {
      // The incoming HTTP handler uses the WASI adapter's own resources, not value conversion.
      if (entry.contract !== HTTP_HANDLER) check(`${entry.binding}#${fn}`, signature);
      promising.push(`${entry.binding}#${fn}`);
    }
  return { suspending, promising };
}

/**
 * Prepare a verified compiled artifact for browsers into the build-owned `directory`.
 * `artifact` must be the exact bytes the application will load; `sha256` is their digest.
 */
export async function prepareBrowserComponent(
  artifact: Uint8Array,
  sha256: string,
  directory: string,
): Promise<BrowserAdapter> {
  const engine = await import("@syntropika/tembra-engine-node");
  if (createHash("sha256").update(artifact).digest("hex") !== sha256)
    throw new Error("Browser preparation received an artifact digest for other bytes");
  // Full native verification of the real component types against the contract.
  const verified = new engine.NodeArtifact(artifact);
  const identity = JSON.parse(verified.description()) as {
    name: string;
    apiVersion: number;
    kind: string;
  };
  if (identity.kind !== "component")
    throw new Error("Only compiled component artifacts need browser preparation");
  const graph = query<Described>(verified, { kind: "describe" });
  const imports = engine.componentImports(artifact);
  if (new Set(imports).size !== imports.length)
    throw new Error("The component imports one name twice");
  const { suspending, promising } = plan(graph, imports);
  const bindgen = await generator();
  const output = bindgen.generate(artifact, {
    name: "orb",
    noTypescript: true,
    instantiation: { tag: "async" },
    noNodejsCompat: true,
    base64Cutoff: 0,
    // JCO drops import versions by default; exact entries keep every versioned key apart.
    map: imports.map((name): [string, string] => [name, name]),
    asyncMode: { tag: "jspi", val: { imports: suspending, exports: promising } },
    emitConditionalSuspendingWasm: true,
  });
  let glue: string | undefined;
  const cores: { name: string; pages?: number; bytes: Uint8Array }[] = [];
  for (const [name, bytes] of output.files) {
    if (name === "orb.js") glue = prepareGlue(new TextDecoder().decode(bytes));
    else if (COMPONENT_CORE.test(name)) {
      const core = engine.instrumentCore(bytes, PREPARED_MAXIMUM_PAGES);
      cores.push({ name, pages: core.initialPages, bytes: core.bytes });
    } else if (TRAMPOLINE_CORE.test(name)) cores.push({ name, bytes });
    else throw new Error(`Unexpected generated file ${name}`);
  }
  if (!glue) throw new Error("The bindings generator produced no glue");
  if (!cores.some((core) => core.pages !== undefined))
    throw new Error("The bindings generator produced no component core module");
  const files = new Map<string, string | Uint8Array>([["glue.js", glue]]);
  const entries = cores.map((core) => {
    const fields = `name: ${JSON.stringify(core.name)}${
      core.pages === undefined ? "" : `, pages: ${core.pages}`
    }`;
    if (core.bytes.byteLength <= INLINE_BYTES)
      return `{ ${fields}, load: () => bytes(${JSON.stringify(Buffer.from(core.bytes).toString("base64"))}) }`;
    const hash = createHash("sha256").update(core.bytes).digest("hex").slice(0, 16);
    const file = `${core.name.slice(0, -".wasm".length)}.${hash}.wasm`;
    files.set(file, core.bytes);
    return `{ ${fields}, load: () => fetch(new URL(${JSON.stringify(`./${file}`)}, import.meta.url)) }`;
  });
  const description: Omit<PreparedComponentSource, "cores" | "instantiate"> = {
    format: PREPARED_FORMAT,
    generator: PREPARED_GENERATOR,
    orb: { name: identity.name, apiVersion: identity.apiVersion },
    artifact: sha256,
    maximumPages: PREPARED_MAXIMUM_PAGES,
    imports,
    suspending,
    promising,
  };
  const fields = Object.entries(description)
    .map(([key, value]) => `  ${key}: ${JSON.stringify(value)},`)
    .join("\n");
  files.set(
    "index.js",
    `// Generated by Tembra tooling; do not edit.
// Host-trusted application code: import this module and register its default export with
// createHost({ isolated: { components: [...] } }). It runs only the artifact with digest
// ${sha256}.
import { preparedComponent } from "@syntropika/tembra-sdk/browser";
import { instantiate } from "./glue.js";

const bytes = (text) => Uint8Array.from(atob(text), (character) => character.charCodeAt(0));

export default preparedComponent({
${fields}
  cores: [
    ${entries.join(",\n    ")},
  ],
  instantiate,
});
`,
  );
  files.set(
    "index.d.ts",
    `declare const component: import("@syntropika/tembra-sdk/browser").PreparedComponent;
export default component;
`,
  );
  const temporary = `${directory}.${process.pid}.tmp`;
  await rm(temporary, { recursive: true, force: true });
  await mkdir(temporary, { recursive: true });
  try {
    for (const [name, contents] of files) await writeFile(path.join(temporary, name), contents);
    await rm(directory, { recursive: true, force: true });
    await rename(temporary, directory);
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
  return {
    module: path.join(directory, "index.js"),
    files: [...files.keys()].map((name) => path.join(directory, name)),
  };
}
