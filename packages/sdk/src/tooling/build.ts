import { createHash, randomUUID } from "node:crypto";
import { access, lstat, mkdir, readFile, readdir, rename, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import ts from "typescript";

import { createArtifact } from "./artifact.js";
import {
  canonicalSdk,
  checkedBundle,
  collectBundleInputs,
  rejectHostBuiltins,
  facetResolution,
  bundleSourceName,
  stableBundleSource,
} from "./bundler.js";
import { generateBindings } from "./codegen.js";
import { resolveConfig } from "./config.js";
import { requiresServerEncoding, serverEncodingPrelude } from "./encoding.js";
import { ToolingError } from "./error.js";
import type {
  BuildOptions,
  BuildResult,
  BuiltArtifact,
  BuildTarget,
  ResolvedConfig,
  ToolingConfig,
} from "./types.js";

type Pending = {
  artifact: BuiltArtifact;
  files: Readonly<Record<string, string>>;
  directory: string;
};
function cancelled(signal?: AbortSignal): void {
  if (signal?.aborted === true) {
    throw new ToolingError("cancelled", "aborted", "build was cancelled", undefined, {
      cause: signal.reason,
    });
  }
}
function diagnostics(errors: readonly ts.Diagnostic[], target?: string): void {
  const failures = errors.filter((error) => error.category === ts.DiagnosticCategory.Error);
  if (failures.length > 0) {
    throw new ToolingError(
      "typecheck",
      "diagnostics",
      ts.formatDiagnostics(failures, {
        getCurrentDirectory: () => process.cwd(),
        getCanonicalFileName: (name) => name,
        getNewLine: () => "\n",
      }),
      target,
    );
  }
}
async function joined<T>(operations: Promise<T>[]): Promise<T[]> {
  const results = await Promise.allSettled(operations);
  const values: T[] = [];
  for (const result of results) {
    if (result.status === "rejected") {
      throw result.reason;
    }
    values.push(result.value);
  }
  return values;
}
async function exists(file: string): Promise<boolean> {
  try {
    await access(file);
    return true;
  } catch {
    return false;
  }
}
function ancestors(start: string): string[] {
  let directory = start;
  const result = [directory];
  let parent = path.dirname(directory);
  while (parent !== directory) {
    result.push(parent);
    directory = parent;
    parent = path.dirname(directory);
  }
  return result;
}
async function existing(files: readonly string[]): Promise<string[]> {
  const present = await Promise.all(files.map(exists));
  return files.filter((_file, index) => present[index] === true);
}
async function configurationInputs(config: ResolvedConfig): Promise<string[]> {
  const manifests = ancestors(config.root).flatMap((directory) =>
    ["package.json", "bun.lock"].map((name) => path.join(directory, name)),
  );
  return [
    ...new Set([
      config.tsconfig,
      ...(config.configDependencies ?? []),
      ...(config.configFile === undefined ? [] : [config.configFile]),
      ...(await existing(manifests)),
    ]),
  ].toSorted();
}
async function packageInputs(files: readonly string[]): Promise<string[]> {
  const directories = [...new Set(files.flatMap((file) => ancestors(path.dirname(file))))];
  return await existing(directories.map((directory) => path.join(directory, "package.json")));
}
function typecheck(
  config: ResolvedConfig,
  entry: string,
  target: string,
  conditions: string[],
): string[] {
  const inputs = new Set<string>([config.tsconfig]);
  const rootNames = [entry];
  const read = (file: string): string | undefined => {
    inputs.add(path.resolve(file));
    return ts.sys.readFile(file);
  };
  let options: ts.CompilerOptions = {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.ESNext,
    moduleResolution: ts.ModuleResolutionKind.Bundler,
    strict: true,
    skipLibCheck: true,
    lib: ["lib.es2022.d.ts", "lib.dom.d.ts"],
    types: [],
  };
  if (ts.sys.fileExists(config.tsconfig)) {
    const loaded = ts.readConfigFile(config.tsconfig, read);
    if (loaded.error !== undefined) {
      diagnostics([loaded.error], target);
    }
    const parsed = ts.parseJsonConfigFileContent(
      loaded.config,
      { ...ts.sys, readFile: read },
      path.dirname(config.tsconfig),
    );
    // Check the selected entry graph and ambient declarations, independently of unrelated roots.
    diagnostics(
      parsed.errors.filter((error) => error.code !== 18_003),
      target,
    );
    options = { ...options, ...parsed.options };
    rootNames.push(...parsed.fileNames.filter((file) => /\.d\.[cm]?ts$/u.test(file)));
  }
  const sdkTypes = fileURLToPath(import.meta.resolve("@tembra/sdk")).replace(/\.js$/u, ".d.ts");
  const browserTypes = fileURLToPath(import.meta.resolve("@tembra/sdk/browser")).replace(
    /\.js$/u,
    ".d.ts",
  );
  const compilerDirectory = path.dirname(fileURLToPath(import.meta.resolve("typescript")));
  const host = ts.createCompilerHost(options);
  host.getDefaultLibLocation = () => compilerDirectory;
  host.getDefaultLibFileName = (compilerOptions) =>
    path.join(compilerDirectory, ts.getDefaultLibFileName(compilerOptions));
  const program = ts.createProgram(
    rootNames,
    {
      ...options,
      paths: {
        ...options.paths,
        "@tembra/sdk": [sdkTypes],
        "@tembra/sdk/browser": [browserTypes],
      },
      ...([
        ts.ModuleResolutionKind.Bundler,
        ts.ModuleResolutionKind.Node16,
        ts.ModuleResolutionKind.NodeNext,
      ].includes(options.moduleResolution ?? ts.ModuleResolutionKind.Bundler)
        ? { customConditions: [...new Set([...(options.customConditions ?? []), ...conditions])] }
        : {}),
      noEmit: true,
      incremental: false,
      composite: false,
    },
    host,
  );
  diagnostics(ts.getPreEmitDiagnostics(program), target);
  for (const file of program.getSourceFiles()) {
    if (!program.isSourceFileDefaultLibrary(file)) {
      inputs.add(path.resolve(file.fileName));
    }
  }
  return [...inputs];
}
function mapSourceRoot(config: ResolvedConfig, target: string): string {
  const relative = path.relative(path.join(config.outDir, target, "__artifact__"), config.root);
  return path.isAbsolute(relative)
    ? pathToFileURL(`${config.root}${path.sep}`).href
    : `${relative.split(path.sep).join("/")}/`;
}
type Bundle = { source: string; map?: string; inputs: string[] };
async function bundle(
  config: ResolvedConfig,
  name: string,
  target: BuildTarget,
  conditions: string[],
): Promise<Bundle> {
  try {
    const inputs = new Set<string>();
    const options: Bun.BuildConfig = {
      throw: false,
      metafile: true,
      root: config.root,
      entrypoints: [path.resolve(config.root, target.entry)],
      naming: "facet.[ext]",
      format: "esm",
      // Browser code generation avoids Node require/import.meta shims. Server resolution is neutral.
      target: "browser",
      minify: target.minify ?? false,
      sourcemap: target.sourcemap === true ? "external" : "none",
      conditions: [...conditions, "import", "default"],
      define: {
        // Browser code generation must not silently supply Node environment globals to server code.
        ...(target.platform === "server" ? { "process.env.NODE_ENV": "process.env.NODE_ENV" } : {}),
        ...target.define,
      },
      ...((await exists(config.tsconfig)) ? { tsconfig: config.tsconfig } : {}),
      plugins: [
        canonicalSdk(),
        rejectHostBuiltins(),
        facetResolution(config.tsconfig, conditions, inputs, target.platform),
      ],
    };
    let result = checkedBundle(await Bun.build(options), config.root);
    let output = result.outputs.find((file) => file.kind === "entry-point");
    if (output === undefined) {
      throw new ToolingError("bundle", "output", "bundler did not return the ESM source", name);
    }
    if (target.platform === "server" && requiresServerEncoding(await output.text())) {
      // Keep TextEncoder as an unbound bundler symbol so authored bindings are renamed safely.
      // Bun owns the banner/map offsets; helper functions remain private to its lexical closure.
      result = checkedBundle(
        await Bun.build({ ...options, banner: serverEncodingPrelude() }),
        config.root,
      );
      output = result.outputs.find((file) => file.kind === "entry-point");
      if (output === undefined) {
        throw new ToolingError("bundle", "output", "bundler did not return the ESM source", name);
      }
    }
    const unsupported = result.outputs.find((file) => file !== output && file.kind !== "sourcemap");
    if (unsupported !== undefined) {
      throw new ToolingError(
        "bundle",
        "output",
        `facet bundles cannot publish asset output ${path.basename(unsupported.path)}`,
        name,
      );
    }
    collectBundleInputs(result, inputs);
    const source = stableBundleSource(
      await output.text(),
      config.root,
      target.sourcemap === true,
      inputs,
    );
    const mapping = result.outputs.find((file) => file.kind === "sourcemap");
    let map: string | undefined;
    if (mapping !== undefined) {
      const value: unknown = JSON.parse(await mapping.text());
      if (
        value === null ||
        typeof value !== "object" ||
        !("sources" in value) ||
        !Array.isArray(value.sources)
      ) {
        throw new Error("Bundler returned an invalid source map");
      }
      const normalized = {
        ...value,
        sources: value.sources.map((filename: unknown) => {
          if (typeof filename !== "string") {
            throw new TypeError("Bundler returned an invalid source map source");
          }
          return bundleSourceName(filename, config.root);
        }),
        sourceRoot: mapSourceRoot(config, name),
      };
      Reflect.deleteProperty(normalized, "debugId");
      map = JSON.stringify(normalized);
    }
    return { source, ...(map === undefined ? {} : { map }), inputs: [...inputs] };
  } catch (error) {
    if (error instanceof ToolingError) {
      throw error;
    }
    throw new ToolingError(
      "bundle",
      "build",
      `could not bundle ${name}: ${error instanceof Error ? error.message : String(error)}`,
      name,
      { cause: error },
    );
  }
}

async function buildTarget(
  config: ResolvedConfig,
  name: string,
  target: BuildTarget,
  common: readonly string[],
  signal?: AbortSignal,
): Promise<Pending> {
  cancelled(signal);
  const conditions = [
    ...new Set([
      ...(target.platform === "browser" ? ["browser"] : []),
      ...(target.conditions ?? []),
    ]),
  ];
  const checked = typecheck(config, path.resolve(config.root, target.entry), name, conditions);
  const bundled = await bundle(config, name, target, conditions);
  cancelled(signal);
  const { source, map } = bundled;
  const selectedExport = target.export ?? "default";
  const artifact = await createArtifact(
    source,
    target.identity ?? name,
    selectedExport,
    target.platform,
    target.maxBytes,
    name,
  );
  // Include mapping metadata in the immutable directory identity.
  const digest = createHash("sha256")
    .update(JSON.stringify(artifact))
    .update(map ?? "")
    .digest("hex");
  const directory = path.join(config.outDir, name, digest);
  const sourceInputs = [...checked, ...bundled.inputs];
  const inputs = [
    ...new Set([...common, ...sourceInputs, ...(await packageInputs(sourceInputs))]),
  ].toSorted();
  return {
    directory,
    files: {
      "facet.js": source,
      "artifact.json": `${JSON.stringify(artifact)}\n`,
      ...(map === undefined ? {} : { "facet.js.map": map }),
    },
    artifact: {
      target: name,
      platform: target.platform,
      artifact,
      selectedExport,
      sourcePath: path.join(directory, "facet.js"),
      artifactPath: path.join(directory, "artifact.json"),
      ...(map === undefined ? {} : { sourcemapPath: path.join(directory, "facet.js.map") }),
      bytes: Buffer.byteLength(source),
      inputs,
    },
  };
}
async function publish(output: Pending, signal?: AbortSignal): Promise<void> {
  const temporary = path.join(path.dirname(output.directory), `.stage-${randomUUID()}`);
  try {
    cancelled(signal);
    if (await reusable(output)) {
      return;
    }
    await mkdir(temporary, { recursive: true });
    await joined(
      Object.entries(output.files).map(async ([name, text]) => {
        await writeFile(path.join(temporary, name), text, "utf-8");
      }),
    );
    cancelled(signal);
    try {
      await rename(temporary, output.directory);
    } catch (error) {
      if (!(await reusable(output))) {
        throw error;
      }
    }
  } catch (error) {
    if (error instanceof ToolingError) {
      throw error;
    }
    throw new ToolingError(
      "write",
      "publication",
      `could not publish ${output.artifact.target}`,
      output.artifact.target,
      { cause: error },
    );
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
/** Returns true only for an existing complete, matching publication; never repairs foreign output. */
async function reusable(output: Pending): Promise<boolean> {
  let metadata;
  try {
    metadata = await lstat(output.directory);
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") {
      return false;
    }
    throw error;
  }
  const mismatch = (): never => {
    throw new ToolingError(
      "write",
      "existing_output",
      `existing artifact directory is incomplete or modified: ${output.directory}`,
      output.artifact.target,
    );
  };
  if (!metadata.isDirectory()) {
    mismatch();
  }
  const entries = await readdir(output.directory);
  const expected = Object.keys(output.files);
  if (entries.length !== expected.length || entries.some((name) => !expected.includes(name))) {
    mismatch();
  }
  await joined(
    Object.entries(output.files).map(async ([name, content]) => {
      const filename = path.join(output.directory, name);
      const fileMetadata = await lstat(filename);
      if (!fileMetadata.isFile()) {
        mismatch();
      }
      const contents = await readFile(filename);
      if (!contents.equals(Buffer.from(content, "utf-8"))) {
        mismatch();
      }
    }),
  );
  return true;
}
/**
 * Generate bindings, typecheck selected entry graphs and publish checked closed ESM for V8/browser.
 * Use this Bun-only API for production builds or to supply artifacts to an existing embedding.
 * It captures validated configuration before yielding, runs generation first and checks every target
 * before publishing artifacts. Artifact publication does not install a facet or request a reload.
 *
 * Output directories are named by content digest. Reuse verifies their exact files and bytes;
 * modified or incomplete output rejects with ToolingError write/existing_output without repair.
 * Typecheck, bundle and loader-profile failures publish no new artifact directories, but generation
 * can already have written outputs. Publication is per directory: write failure or cancellation can
 * leave earlier directories published. Cancellation joins locally owned work and does not undo writes.
 *
 * Paths resolve against root; outDir defaults to dist/tembra. Given src/facet.ts exports a default
 * facet, this creates a server bundle and shows the envelope path to pass to an embedding:
 *
 * @example
 * ```ts
 * import { build, defineConfig } from "@tembra/sdk/tooling";
 * const config = defineConfig({
 *   builds: { service: { entry: "src/facet.ts", platform: "server" } },
 * });
 * const result = await build(config);
 * for (const output of result.artifacts) {
 *   console.log(output.artifactPath, output.artifact.sha256);
 * }
 * ```
 *
 * Use the returned artifacts with an embedding; `dev` delivers updates through explicit host adapters.
 */
export async function build(
  config: ToolingConfig | ResolvedConfig,
  options: BuildOptions = {},
): Promise<BuildResult> {
  const { signal } = options;
  cancelled(signal);
  const resolved = resolveConfig(config);
  const names =
    options.targets === undefined
      ? Object.keys(resolved.builds).toSorted()
      : [...new Set(options.targets)];
  const selected = names.map((name) => {
    const target = resolved.builds[name];
    if (target === undefined) {
      throw new ToolingError("config", "target", `unknown build target ${name}`, name);
    }
    return { name, target };
  });
  const generated = await generateBindings(resolved, signal === undefined ? {} : { signal });
  cancelled(signal);
  const common = [
    ...(await configurationInputs(resolved)),
    ...generated.inputs,
    ...generated.files,
  ];
  const pending = await joined(
    selected.map(
      async ({ name, target }) => await buildTarget(resolved, name, target, common, signal),
    ),
  );
  cancelled(signal);
  // Publication starts only after every target passes; each rename exposes a complete directory.
  await joined(
    pending.map(async (output) => {
      await publish(output, signal);
    }),
  );
  return {
    artifacts: pending.map((output) => output.artifact),
    generated,
    inputs: [
      ...new Set([...common, ...pending.flatMap((output) => output.artifact.inputs)]),
    ].toSorted(),
  };
}
