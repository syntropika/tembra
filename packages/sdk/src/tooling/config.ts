import { randomUUID } from "node:crypto";
import { realpathSync } from "node:fs";
import { access, readFile, unlink } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

import ts from "typescript";

import { canonicalSdk, checkedBundle, collectBundleInputs } from "./bundler.js";
import { ToolingError } from "./error.js";
import type { BindingConfig, BuildTarget, ResolvedConfig, ToolingConfig } from "./types.js";

function invalid(message: string): never {
  throw new ToolingError("config", "invalid_config", message);
}
function record<T>(value: T, label: string): asserts value is T & Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    invalid(`${label} must be an object`);
  }
}
function text<T>(value: T, label: string): asserts value is T & string {
  if (typeof value !== "string" || !value.trim()) {
    invalid(`${label} must be a nonempty string`);
  }
}
function array<T>(value: T, label: string): asserts value is T & readonly unknown[] {
  if (!Array.isArray(value)) {
    invalid(`${label} must be an array`);
  }
}
function strings<T>(value: T, label: string, nonempty = false): asserts value is T & string[] {
  if (!Array.isArray(value) || (nonempty && !value.length)) {
    invalid(`${label} must be an array of strings`);
  }
  for (const item of value) {
    text(item, label);
  }
}
function mapping<T>(
  value: T,
  label: string,
): asserts value is T & Readonly<Record<string, string>> {
  record(value, label);
  for (const item of Object.values(value)) {
    text(item, label);
  }
}

function resolveTarget(name: string, target: BuildTarget, base: string): BuildTarget {
  if (!/^[A-Za-z0-9_-]+$/u.test(name)) {
    invalid(`invalid build name: ${name}`);
  }
  record(target, `build ${name}`);
  text(target.entry, `${name}.entry`);
  if (target.platform !== "server" && target.platform !== "browser") {
    invalid(`${name}.platform must be server or browser`);
  }
  for (const key of ["identity", "export"] as const) {
    if (target[key] !== undefined) {
      text(target[key], `${name}.${key}`);
    }
  }
  for (const key of ["minify", "sourcemap"] as const) {
    if (target[key] !== undefined && typeof target[key] !== "boolean") {
      invalid(`${name}.${key} must be a boolean`);
    }
  }
  if (
    target.maxBytes !== undefined &&
    (!Number.isSafeInteger(target.maxBytes) || target.maxBytes <= 0)
  ) {
    invalid(`${name}.maxBytes must be a positive safe integer`);
  }
  if (target.define !== undefined) {
    mapping(target.define, `${name}.define`);
  }
  if (target.conditions !== undefined) {
    strings(target.conditions, `${name}.conditions`);
  }
  return {
    ...target,
    entry: path.resolve(base, target.entry),
    ...(target.define === undefined ? {} : { define: { ...target.define } }),
    ...(target.conditions === undefined ? {} : { conditions: [...target.conditions] }),
  };
}

function resolveBinding(binding: BindingConfig, index: number, base: string): BindingConfig {
  record(binding, `codegen[${index}]`);
  record(binding.source, `codegen[${index}].source`);
  const { source } = binding;
  if (["descriptor", "module", "command"].filter((key) => key in source).length !== 1) {
    invalid("codegen source requires exactly one descriptor, module, or command");
  }
  let resolvedSource: BindingConfig["source"];
  if ("descriptor" in source) {
    text(source.descriptor, "source.descriptor");
    resolvedSource = { descriptor: path.resolve(base, source.descriptor) };
  } else if ("module" in source) {
    text(source.module, "source.module");
    text(source.export, "source.export");
    resolvedSource = { module: path.resolve(base, source.module), export: source.export };
  } else {
    strings(source.command, "source.command", true);
    strings(source.watch, "source.watch");
    resolvedSource = {
      command: [...source.command],
      watch: source.watch.map((file) => path.resolve(base, file)),
    };
  }
  if (binding.name !== undefined) {
    text(binding.name, "binding.name");
  }
  if (binding.nativeTypes !== undefined) {
    mapping(binding.nativeTypes, "binding.nativeTypes");
  }
  if (binding.rustCommand !== undefined) {
    strings(binding.rustCommand, "binding.rustCommand", true);
  }
  const outputs: Record<string, string> = {};
  for (const key of ["descriptor", "typescript", "rust"] as const) {
    if (binding[key] !== undefined) {
      text(binding[key], `binding.${key}`);
      outputs[key] = path.resolve(base, binding[key]);
    }
  }
  if (!Object.keys(outputs).length) {
    invalid("each binding requires at least one generated output");
  }
  return {
    ...binding,
    ...outputs,
    source: resolvedSource,
    ...(binding.nativeTypes === undefined ? {} : { nativeTypes: { ...binding.nativeTypes } }),
    ...(binding.rustCommand === undefined ? {} : { rustCommand: [...binding.rustCommand] }),
  };
}

function resolveDevelopment(dev: ToolingConfig["dev"]): ResolvedConfig["dev"] {
  if (dev !== undefined) {
    record(dev, "dev");
    for (const key of ["pollInterval", "debounce"] as const) {
      const value = dev[key];
      if (
        value !== undefined &&
        (!Number.isSafeInteger(value) || value < (key === "pollInterval" ? 1 : 0))
      ) {
        invalid(`dev.${key} must be a valid integer interval`);
      }
    }
  }
  return {
    ...(dev?.pollInterval === undefined ? {} : { pollInterval: dev.pollInterval }),
    ...(dev?.debounce === undefined ? {} : { debounce: dev.debounce }),
  };
}

function physicalPath(file: string): string {
  let existing = path.resolve(file);
  const suffix: string[] = [];
  for (;;) {
    try {
      return path.join(realpathSync(existing), ...suffix);
    } catch (error) {
      const missing =
        error !== null &&
        typeof error === "object" &&
        "code" in error &&
        (error.code === "ENOENT" || error.code === "ENOTDIR");
      const parent = path.dirname(existing);
      if (!missing || parent === existing) {
        throw new ToolingError(
          "config",
          "path_resolution_failed",
          `Cannot resolve output/input path: ${file}`,
          undefined,
          { cause: error },
        );
      }
      suffix.unshift(path.basename(existing));
      existing = parent;
    }
  }
}

/** Check known source paths and output destinations, following existing symlink ancestors. */
export function assertOutputPaths(config: ResolvedConfig, inputs?: Iterable<string>): void {
  const authored = new Set(Object.values(config.builds).map((target) => target.entry));
  for (const binding of config.codegen) {
    if ("descriptor" in binding.source) {
      authored.add(binding.source.descriptor);
    } else if ("module" in binding.source) {
      authored.add(binding.source.module);
    } else {
      for (const file of binding.source.watch) {
        authored.add(file);
      }
    }
  }
  authored.add(config.tsconfig);
  for (const file of [...(inputs ?? []), ...(config.configDependencies ?? [])]) {
    authored.add(file);
  }
  if (config.configFile !== undefined) {
    authored.add(config.configFile);
  }
  const outputs = new Set<string>();
  const sources = new Set([...authored].map(physicalPath));
  for (const binding of config.codegen) {
    for (const file of [binding.descriptor, binding.typescript, binding.rust]) {
      if (file === undefined) {
        continue;
      }
      const destination = physicalPath(file);
      if (outputs.has(destination) || sources.has(destination)) {
        const message = `generated output must be unique and distinct from authored sources: ${file}`;
        if (inputs !== undefined) {
          throw new ToolingError("config", "output_collision", message);
        }
        invalid(message);
      }
      outputs.add(destination);
    }
  }
}

/**
 * Validate project settings at declaration time, preserving inferred target names and literal options.
 * Returns the original object; consumers capture independent settings with {@link resolveConfig}.
 * Invalid settings throw a configuration `ToolingError` synchronously.
 *
 * @example
 * ```ts
 * import { defineConfig } from "@tembra/sdk/tooling";
 * export default defineConfig({
 *   builds: { server: { entry: "src/server.ts", platform: "server" } },
 * });
 * ```
 */
export function defineConfig<T extends ToolingConfig>(config: T): T {
  resolveConfig(config);
  return config;
}

/**
 * Validate settings and capture independent copies of their nested option collections.
 * `root` defaults to the working directory; `config.root` resolves against it. All other paths
 * resolve against that project root. Output and tsconfig defaults are `dist/tembra` and
 * `tsconfig.json`; omitted codegen and development settings become empty collections.
 * Throws a configuration `ToolingError` before returning if a known setting or output is invalid.
 * Retains loaded configuration provenance when resolving an already resolved configuration.
 * Existing symlinks and output ancestors are resolved for collision checks, including absent leaves.
 * These filesystem observations are not a lock against concurrent path or symlink changes.
 */
export function resolveConfig(
  config: ToolingConfig | ResolvedConfig,
  root = process.cwd(),
): ResolvedConfig {
  record(config, "config");
  if (config.root !== undefined) {
    text(config.root, "root");
  }
  const base = path.resolve(root, config.root ?? ".");
  record(config.builds, "builds");
  const builds = Object.fromEntries(
    Object.entries(config.builds).map(([name, target]): [string, BuildTarget] => [
      name,
      resolveTarget(name, target, base),
    ]),
  );
  for (const key of ["outDir", "tsconfig"] as const) {
    if (config[key] !== undefined) {
      text(config[key], key);
    }
  }
  if (config.codegen !== undefined) {
    array(config.codegen, "codegen");
  }
  const codegen = (config.codegen ?? []).map((binding, index) =>
    resolveBinding(binding, index, base),
  );
  const dev = resolveDevelopment(config.dev);
  let configDependencies: readonly string[] | undefined;
  if ("configDependencies" in config) {
    strings(config.configDependencies, "configDependencies");
    ({ configDependencies } = config);
  }
  const resolved: ResolvedConfig = {
    root: base,
    builds,
    outDir: path.resolve(base, config.outDir ?? "dist/tembra"),
    tsconfig: path.resolve(base, config.tsconfig ?? "tsconfig.json"),
    codegen,
    dev,
    ...("configFile" in config && typeof config.configFile === "string"
      ? { configFile: config.configFile }
      : {}),
    ...(configDependencies === undefined ? {} : { configDependencies: [...configDependencies] }),
  };
  assertOutputPaths(resolved);
  return resolved;
}

function trustedModuleInputs(file: string, files: readonly string[]): string[] {
  const inputs = new Set(files.map((input) => path.resolve(path.dirname(file), input)));
  const configs = new Set<string>();
  for (const input of inputs) {
    const config = ts.findConfigFile(path.dirname(input), (candidate) =>
      ts.sys.fileExists(candidate),
    );
    if (config !== undefined) {
      configs.add(config);
    }
  }
  const readConfig = (input: string): string | undefined => {
    const content = ts.sys.readFile(input);
    if (content !== undefined) {
      inputs.add(path.resolve(input));
    }
    return content;
  };
  for (const config of configs) {
    const parsed = ts.readConfigFile(config, readConfig);
    if (parsed.error === undefined) {
      ts.parseJsonConfigFileContent(
        parsed.config,
        { ...ts.sys, readFile: readConfig },
        path.dirname(config),
      );
    }
  }
  return [...inputs];
}

function trustedModuleOutput(result: Bun.BuildOutput): Bun.BuildArtifact {
  const output = result.outputs.find((item) => item.kind === "entry-point");
  if (output === undefined) {
    throw new ToolingError("config", "output", "Trusted module bundling returned no entrypoint");
  }
  const unsupported = result.outputs.find((item) => item !== output && item.kind !== "sourcemap");
  if (unsupported !== undefined) {
    throw new ToolingError(
      "config",
      "output",
      `Trusted modules cannot publish asset output ${path.basename(unsupported.path)}`,
    );
  }
  return output;
}

/**
 * Evaluate a trusted absolute module path with a fresh bundled local dependency graph.
 * Package imports retain Bun's normal identity/cache. Returned inputs are absolute authored paths.
 * Executes project code with Bun authority; removes its temporary module after evaluation.
 * Transpilation, evaluation and filesystem failures propagate with their original error values.
 * Inlined text/data imports are supported; emitted asset files are refused before evaluation.
 */
export async function loadTrustedModule(
  file: string,
): Promise<{ exports: Record<string, unknown>; inputs: string[] }> {
  const temporary = path.join(path.dirname(file), `.tembra-${randomUUID()}.mjs`);
  try {
    const inputs = new Set<string>();
    const result = checkedBundle(
      await Bun.build({
        throw: false,
        metafile: true,
        root: path.dirname(file),
        entrypoints: [file],
        naming: path.basename(temporary),
        packages: "external",
        target: "bun",
        format: "esm",
        sourcemap: "inline",
        plugins: [canonicalSdk(true)],
      }),
    );
    collectBundleInputs(result, inputs);
    const output = trustedModuleOutput(result);
    await Bun.write(temporary, output);
    const namespace: unknown = await import(pathToFileURL(temporary).href);
    record(namespace, "module namespace");
    return { exports: namespace, inputs: trustedModuleInputs(file, [...inputs]) };
  } finally {
    await unlink(temporary).catch(() => {
      /* The file may not exist after a failed transpilation. */
    });
  }
}

/** Discover local module inputs without evaluating project code or writing transpiled output. */
export async function inspectTrustedModuleInputs(file: string): Promise<string[]> {
  const inputs = new Set<string>();
  const result = checkedBundle(
    await Bun.build({
      throw: false,
      metafile: true,
      root: path.dirname(file),
      entrypoints: [file],
      packages: "external",
      target: "bun",
      format: "esm",
      plugins: [canonicalSdk(true)],
    }),
  );
  collectBundleInputs(result, inputs);
  trustedModuleOutput(result);
  return trustedModuleInputs(file, [...inputs]);
}

async function loadConfigurationModule(
  file: string,
): Promise<{ exports: Record<string, unknown>; inputs: string[] }> {
  if (!file.endsWith(".json")) {
    return await loadTrustedModule(file);
  }
  const value: unknown = JSON.parse(await readFile(file, "utf-8"));
  return { exports: { default: value }, inputs: [file] };
}

/**
 * Load trusted executable project settings, or parse a JSON configuration, and resolve their paths.
 * `file` is relative to the working directory. When omitted, selects the first existing
 * `tembra.config.ts`, `.mts`, `.js`, `.mjs` or `.json` there, in that order. The selected file's
 * directory anchors `root`. Local module dependencies are evaluated afresh; packages retain Bun
 * identity. Configuration code executes with Bun authority. Failures throw a configuration
 * `ToolingError` retaining the original cause; no generated output is written.
 *
 * @example
 * ```ts
 * import { build, loadConfig } from "@tembra/sdk/tooling";
 * const config = await loadConfig("./tembra.config.ts");
 * const result = await build(config);
 * console.log(result.artifacts.map(output => output.artifactPath));
 * ```
 */
export async function loadConfig(file?: string): Promise<ResolvedConfig> {
  try {
    let selected = file === undefined ? undefined : path.resolve(file);
    if (selected === undefined) {
      const candidates = [
        "tembra.config.ts",
        "tembra.config.mts",
        "tembra.config.js",
        "tembra.config.mjs",
        "tembra.config.json",
      ].map((candidate) => path.resolve(candidate));
      const available = await Promise.all(
        candidates.map(async (candidate) => {
          try {
            await access(candidate);
            return candidate;
          } catch {
            return undefined;
          }
        }),
      );
      selected = available.find((candidate) => candidate !== undefined);
    }
    if (selected === undefined) {
      throw new ToolingError("config", "missing_config", "No tembra.config file was found");
    }
    const loaded = await loadConfigurationModule(selected);
    // SAFETY: resolveConfig validates every known field before constructing the returned resolved configuration.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Select the public validator signature; no asserted input escapes validation.
    const resolved = resolveConfig(loaded.exports.default as ToolingConfig, path.dirname(selected));
    const config = { ...resolved, configFile: selected, configDependencies: loaded.inputs };
    assertOutputPaths(config);
    return config;
  } catch (error) {
    if (error instanceof ToolingError) {
      throw error;
    }
    throw new ToolingError(
      "config",
      "load_failed",
      "Unable to load tooling configuration",
      undefined,
      { cause: error },
    );
  }
}
