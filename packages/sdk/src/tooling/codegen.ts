import { spawn } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants } from "node:fs";
import {
  access,
  mkdir,
  readFile,
  realpath,
  rename,
  stat,
  unlink,
  writeFile,
} from "node:fs/promises";
import path from "node:path";

import { canonicalJson, fromDescriptor, parsePortableJson } from "@tembra/sdk";
import ts from "typescript";

import {
  assertOutputPaths,
  inspectTrustedModuleInputs,
  loadTrustedModule,
  resolveConfig,
} from "./config.js";
import { ToolingError } from "./error.js";
import type { BindingConfig, GeneratedBindings, ResolvedConfig } from "./types.js";
import { generateTypeScript } from "./typescript-generator.js";

const generated = new Map<string, readonly [string, string][]>();
const descriptors = new Map<string, string>();
function cancelled(signal?: AbortSignal): void {
  if (signal?.aborted === true) {
    throw new ToolingError("cancelled", "aborted", "Binding generation was cancelled", undefined, {
      cause: signal.reason,
    });
  }
}

async function command(
  argv: readonly string[],
  root: string,
  input: string,
  signal?: AbortSignal,
): Promise<string> {
  cancelled(signal);
  const [program, ...args] = argv;
  if (program === undefined || program.length === 0) {
    throw new Error("Generator command requires an executable");
  }
  // oxlint-disable-next-line promise/avoid-new -- Join child-process close, pipe errors and cancellation before returning.
  return await new Promise((resolve, reject) => {
    const grouped = process.platform !== "win32";
    const processOptions = { cwd: root, detached: grouped };
    // Exporters consume no input; even an empty pipe write can race a successful fast exit.
    const child =
      input.length === 0
        ? spawn(program, args, { ...processOptions, stdio: ["ignore", "pipe", "pipe"] })
        : spawn(program, args, { ...processOptions, stdio: ["pipe", "pipe", "pipe"] });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let bytes = 0;
    let failure: Error | undefined;
    let inputFailure: Error | undefined;
    let killTimer: ReturnType<typeof setTimeout> | undefined;
    const kill = (kind: NodeJS.Signals) => {
      if (grouped && child.pid !== undefined) {
        try {
          process.kill(-child.pid, kind);
        } catch {
          /* Already exited. */
        }
      } else {
        child.kill(kind);
      }
    };
    const stop = () => {
      kill("SIGTERM");
      killTimer ??= setTimeout(() => {
        kill("SIGKILL");
      }, 1000);
    };
    const abort = () => {
      failure = new Error("Generator was cancelled", { cause: signal?.reason });
      stop();
    };
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted === true) {
      abort();
    }
    child.on("error", (error: Error) => {
      failure = error;
    });
    child.stdout.on("data", (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > 16 * 1024 * 1024) {
        failure = new Error("Generator stdout exceeds 16 MiB");
        stop();
      } else {
        stdout.push(chunk);
      }
    });
    child.stderr.on("data", (chunk: Buffer) => {
      if (stderr.reduce((length, part) => length + part.length, 0) < 64 * 1024) {
        stderr.push(chunk);
      }
    });
    child.stdin?.on("error", (error) => {
      inputFailure ??= error;
    });
    child.on("close", (code: number | null, exitSignal: NodeJS.Signals | null) => {
      signal?.removeEventListener("abort", abort);
      if (killTimer) {
        clearTimeout(killTimer);
      }
      // A completed exporter does not transfer ownership of background descendants.
      if (grouped) {
        kill("SIGKILL");
      }
      const diagnostic = {
        code,
        signal: exitSignal,
        stderr: Buffer.concat(stderr).toString("utf-8"),
      };
      if (failure) {
        reject(
          Object.assign(failure, { exitCode: code, signal: exitSignal, stderr: diagnostic.stderr }),
        );
      } else if (code === 0 && inputFailure === undefined) {
        resolve(Buffer.concat(stdout).toString("utf-8"));
      } else {
        reject(
          Object.assign(
            new Error(
              `Generator exited with ${code ?? exitSignal}${inputFailure === undefined ? "" : `; stdin failed: ${inputFailure.message}`}: ${diagnostic.stderr}`,
              inputFailure === undefined ? undefined : { cause: inputFailure },
            ),
            diagnostic,
          ),
        );
      }
    });
    child.stdin?.end(input);
  });
}

async function writeChanged(file: string, content: string, signal?: AbortSignal): Promise<void> {
  cancelled(signal);
  if (
    await readFile(file, "utf-8").then(
      (previous) => previous === content,
      () => false,
    )
  ) {
    return;
  }
  await mkdir(path.dirname(file), { recursive: true });
  const temporary = `${file}.tembra-${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, content, "utf-8");
    cancelled(signal);
    await rename(temporary, file);
  } finally {
    await unlink(temporary).catch(() => {
      /* A failed write may leave no temporary file. */
    });
  }
}

async function executableCandidate(file: string): Promise<string | undefined> {
  try {
    await access(file, constants.X_OK);
    const metadata = await stat(file);
    return metadata.isFile() ? file : undefined;
  } catch {
    return undefined;
  }
}
async function executable(program: string, root: string): Promise<string | undefined> {
  const candidates =
    program.includes("/") || program.includes("\\")
      ? [path.resolve(root, program)]
      : (process.env.PATH ?? "")
          .split(path.delimiter)
          .map((directory) => path.resolve(root, directory || ".", program));
  const suffixes =
    process.platform === "win32"
      ? ["", ...(process.env.PATHEXT ?? ".EXE;.CMD;.BAT").split(";")]
      : [""];
  const available = await Promise.all(
    candidates.flatMap((candidate) =>
      suffixes.map(async (suffix) => await executableCandidate(candidate + suffix)),
    ),
  );
  return available.find((file) => file !== undefined);
}
async function digest(file: string): Promise<string> {
  return createHash("sha256")
    .update(await readFile(file))
    .digest("hex");
}

type PreparedBinding = {
  binding: BindingConfig;
  canonical: string;
  commandInputs: [string, string][];
  cacheable: boolean;
  descriptorCandidate?: DescriptorCandidate;
};
type DiscoveredBinding = { binding: BindingConfig; inputs: string[] };
type DescriptorCandidate = {
  binding: BindingConfig;
  inputs: string[];
  key: string;
  canonical: string;
};

async function inputFingerprint(file: string): Promise<{
  complete: boolean;
  row: readonly [string, string | null, string];
}> {
  try {
    const physical = await realpath(file);
    const metadata = await stat(physical);
    if (!metadata.isFile()) {
      return { complete: false, row: [file, physical, "unfingerprinted"] };
    }
    return { complete: true, row: [file, physical, await digest(physical)] };
  } catch (error) {
    if (
      error !== null &&
      typeof error === "object" &&
      "code" in error &&
      (error.code === "ENOENT" || error.code === "ENOTDIR")
    ) {
      return { complete: true, row: [file, null, "missing"] };
    }
    throw error;
  }
}

async function descriptorKey(
  binding: BindingConfig,
  config: ResolvedConfig,
  inputs: readonly string[],
): Promise<string | undefined> {
  if ("command" in binding.source) {
    const [program] = binding.source.command;
    if (program === undefined || (await executable(program, config.root)) === undefined) {
      return undefined;
    }
  }
  const fingerprints = await Promise.allSettled(inputs.toSorted().map(inputFingerprint));
  const rows: (readonly [string, string | null, string])[] = [];
  let complete = true;
  for (const fingerprint of fingerprints) {
    if (fingerprint.status === "rejected") {
      throw fingerprint.reason;
    }
    complete &&= fingerprint.value.complete;
    rows.push(fingerprint.value.row);
  }
  if (!complete) {
    return undefined;
  }
  const environment = Object.entries(process.env).toSorted(([left], [right]) =>
    left < right ? -1 : Number(left > right),
  );
  return createHash("sha256")
    .update(JSON.stringify({ root: config.root, cwd: process.cwd(), binding, rows, environment }))
    .digest("hex");
}

async function prepareDescriptor(
  discovered: DiscoveredBinding,
  config: ResolvedConfig,
  inputs: Set<string>,
  commonInputs: readonly string[],
  signal?: AbortSignal,
): Promise<{ canonical: string; descriptorCandidate?: DescriptorCandidate }> {
  const { binding } = discovered;
  const sourceInputs = new Set(discovered.inputs);
  const key = await descriptorKey(binding, config, [
    ...new Set([...commonInputs, ...sourceInputs]),
  ]);
  cancelled(signal);
  const cached = key === undefined ? undefined : descriptors.get(key);
  if (cached !== undefined) {
    return { canonical: canonicalJson(fromDescriptor(parsePortableJson(cached)).descriptor) };
  }
  const canonical = await readDescriptor(binding, config, sourceInputs, signal);
  cancelled(signal);
  for (const file of sourceInputs) {
    inputs.add(file);
  }
  return {
    canonical,
    ...(key === undefined
      ? {}
      : {
          descriptorCandidate: {
            binding,
            canonical,
            key,
            inputs: [...new Set([...commonInputs, ...sourceInputs])],
          },
        }),
  };
}

async function reusableDescriptors(
  bindings: readonly PreparedBinding[],
  config: ResolvedConfig,
  signal?: AbortSignal,
): Promise<DescriptorCandidate[]> {
  const candidates = bindings.flatMap((binding) =>
    binding.descriptorCandidate === undefined ? [] : [binding.descriptorCandidate],
  );
  const checks = await Promise.allSettled(
    candidates.map(async (candidate) => ({
      candidate,
      key: await descriptorKey(candidate.binding, config, candidate.inputs),
    })),
  );
  const reusable: DescriptorCandidate[] = [];
  for (const check of checks) {
    if (check.status === "rejected") {
      throw check.reason;
    }
    if (check.value.key === check.value.candidate.key) {
      reusable.push(check.value.candidate);
    }
  }
  cancelled(signal);
  return reusable;
}

function rememberDescriptor(candidate: DescriptorCandidate): void {
  const oldest = descriptors.keys().next().value;
  if (descriptors.size >= 128 && oldest !== undefined) {
    descriptors.delete(oldest);
  }
  descriptors.set(candidate.key, candidate.canonical);
}
async function readDescriptor(
  binding: BindingConfig,
  config: ResolvedConfig,
  inputs: Set<string>,
  signal?: AbortSignal,
): Promise<string> {
  let descriptor: unknown;
  if ("descriptor" in binding.source) {
    inputs.add(binding.source.descriptor);
    descriptor = parsePortableJson(await readFile(binding.source.descriptor, "utf-8"));
  } else if ("module" in binding.source) {
    const loaded = await loadTrustedModule(binding.source.module);
    for (const file of loaded.inputs) {
      inputs.add(file);
    }
    const token = loaded.exports[binding.source.export];
    if (token === null || typeof token !== "object" || !("descriptor" in token)) {
      throw new Error(`Export ${binding.source.export} must be a contract token with a descriptor`);
    }
    ({ descriptor } = token);
  } else {
    for (const file of binding.source.watch) {
      inputs.add(file);
    }
    const [program] = binding.source.command;
    if (program === undefined) {
      throw new Error("Descriptor command requires an executable");
    }
    const exporter = await executable(program, config.root);
    if (exporter !== undefined) {
      inputs.add(exporter);
    }
    descriptor = parsePortableJson(await command(binding.source.command, config.root, "", signal));
  }
  return canonicalJson(fromDescriptor(descriptor).descriptor);
}
async function fileProvenance(file: string): Promise<[string, string] | undefined> {
  const metadata = await stat(file).catch(() => undefined);
  if (metadata === undefined || !metadata.isFile()) {
    return undefined;
  }
  return [file, await digest(file)];
}
async function prepareBinding(
  discovered: DiscoveredBinding,
  config: ResolvedConfig,
  inputs: Set<string>,
  commonInputs: readonly string[],
  signal?: AbortSignal,
): Promise<PreparedBinding> {
  cancelled(signal);
  const { binding } = discovered;
  const descriptor = await prepareDescriptor(discovered, config, inputs, commonInputs, signal);
  if (binding.rust === undefined) {
    return { binding, ...descriptor, commandInputs: [], cacheable: true };
  }
  const [program, ...args] = binding.rustCommand ?? ["tembra-codegen"];
  if (program === undefined) {
    throw new Error("Rust generator requires an executable");
  }
  const generator = await executable(program, config.root);
  const commandInputs = await Promise.allSettled([
    ...(generator === undefined ? [] : [fileProvenance(generator)]),
    ...args.map(async (arg) => await fileProvenance(path.resolve(config.root, arg))),
  ]);
  const present: [string, string][] = [];
  for (const outcome of commandInputs) {
    if (outcome.status === "rejected") {
      throw outcome.reason;
    }
    if (outcome.value !== undefined) {
      present.push(outcome.value);
    }
  }
  for (const [file] of present) {
    inputs.add(file);
  }
  return { binding, ...descriptor, commandInputs: present, cacheable: generator !== undefined };
}
async function generateOutput(
  prepared: PreparedBinding,
  root: string,
  signal?: AbortSignal,
): Promise<readonly [string, string][]> {
  cancelled(signal);
  const { binding, canonical, commandInputs, cacheable } = prepared;
  const key = createHash("sha256")
    .update(JSON.stringify({ canonical, binding, root, commandInputs }))
    .digest("hex");
  const cached = cacheable ? generated.get(key) : undefined;
  if (cached !== undefined) {
    return cached;
  }
  const outputs: [string, string][] = [];
  if (binding.descriptor !== undefined) {
    outputs.push([binding.descriptor, `${canonical}\n`]);
  }
  if (binding.typescript !== undefined) {
    outputs.push([
      binding.typescript,
      await generateTypeScript(parsePortableJson(canonical), {
        ...(binding.name === undefined ? {} : { name: binding.name }),
        ...(binding.nativeTypes === undefined ? {} : { nativeTypes: binding.nativeTypes }),
      }),
    ]);
  }
  if (binding.rust !== undefined) {
    outputs.push([
      binding.rust,
      await command(binding.rustCommand ?? ["tembra-codegen"], root, canonical, signal),
    ]);
  }
  if (cacheable) {
    cancelled(signal);
    const oldest = generated.keys().next().value;
    if (generated.size >= 128 && oldest !== undefined) {
      generated.delete(oldest);
    }
    generated.set(key, outputs);
  }
  return outputs;
}
function collectTypeConfigInputs(config: ResolvedConfig, inputs: Set<string>): void {
  const readConfigInput = (file: string): string | undefined => {
    const content = ts.sys.readFile(file);
    if (content !== undefined) {
      inputs.add(path.resolve(file));
    }
    return content;
  };
  const tsconfig = ts.readConfigFile(config.tsconfig, readConfigInput);
  if (tsconfig.error === undefined) {
    ts.parseJsonConfigFileContent(
      tsconfig.config,
      { ...ts.sys, readFile: readConfigInput },
      path.dirname(config.tsconfig),
    );
  }
}

async function commandInputPaths(argv: readonly string[], root: string): Promise<string[]> {
  const [program, ...args] = argv;
  const selected = program === undefined ? undefined : await executable(program, root);
  const files = await Promise.allSettled(
    args.map(async (arg) => {
      const file = path.resolve(root, arg);
      const metadata = await stat(file).catch(() => undefined);
      return metadata?.isFile() === true ? file : undefined;
    }),
  );
  const inputs = selected === undefined ? [] : [selected];
  for (const file of files) {
    if (file.status === "rejected") {
      throw file.reason;
    }
    if (file.value !== undefined) {
      inputs.push(file.value);
    }
  }
  return inputs;
}

async function bindingInputPaths(binding: BindingConfig, root: string): Promise<string[]> {
  const operations: Promise<string[]>[] = [];
  const inputs = new Set<string>();
  if ("module" in binding.source) {
    operations.push(inspectTrustedModuleInputs(binding.source.module));
  } else if ("command" in binding.source) {
    operations.push(commandInputPaths(binding.source.command, root));
    for (const file of binding.source.watch) {
      inputs.add(file);
    }
  } else {
    inputs.add(binding.source.descriptor);
  }
  if (binding.rust !== undefined) {
    operations.push(commandInputPaths(binding.rustCommand ?? ["tembra-codegen"], root));
  }
  const outcomes = await Promise.allSettled(operations);
  for (const outcome of outcomes) {
    if (outcome.status === "rejected") {
      throw outcome.reason;
    }
    for (const file of outcome.value) {
      inputs.add(file);
    }
  }
  return [...inputs];
}

async function discoverBindingInputs(
  config: ResolvedConfig,
  inputs: Set<string>,
): Promise<DiscoveredBinding[]> {
  const outcomes = await Promise.allSettled(
    config.codegen.map(async (binding) => ({
      binding,
      inputs: await bindingInputPaths(binding, config.root),
    })),
  );
  const discovered: DiscoveredBinding[] = [];
  for (const outcome of outcomes) {
    if (outcome.status === "rejected") {
      throw outcome.reason;
    }
    discovered.push(outcome.value);
    for (const file of outcome.value.inputs) {
      inputs.add(file);
    }
  }
  return discovered;
}

/**
 * Generate configured descriptor, TypeScript and Rust files without rewriting identical contents.
 * Captures settings before yielding, validates all descriptor graphs and protects authored inputs
 * before writing. No configured bindings returns empty files. Results include absolute output and
 * dependency paths for subsequent watching. Commands run in the project root with explicit argv;
 * descriptor exporters emit JSON, and Rust generators consume JSON stdin and emit Rust stdout.
 * Symlink aliases of sources and other outputs are refused, including absent output leaves.
 * Static local module graphs are inspected before evaluating tokens or running exporter commands.
 * Completed successful generations reuse descriptors while source/configuration contents, declared
 * watch files, executable provenance, environment and binding options remain identical. Unrelated
 * facet edits do not re-execute exporters or token modules. Cache hits still validate descriptors
 * and inspect current aliases/graphs. Directory inputs or unknown executable provenance disable
 * reuse; declare all exporter file dependencies in `watch`. Packages retain Bun's module identity;
 * replace the tooling process after installing changed packages. Runtime module I/O, clocks and other
 * undeclared external state are not cache triggers. Concurrent cold calls are independent.
 * Checks are filesystem observations; callers must keep paths stable during generation. They do
 * not provide a filesystem sandbox or rollback concurrent edits or trusted project-code effects.
 * Exporter/generator failures retain their cause and publish no generated output. File publication
 * is atomic per file; later failure or cancellation does not undo earlier files or command effects.
 * Cancellation waits for command exit and stdio closure, and signals its POSIX process group.
 *
 * @example
 * ```ts
 * import { generateBindings, loadConfig } from "@tembra/sdk/tooling";
 * const generated = await generateBindings(await loadConfig());
 * console.log(generated.files);
 * ```
 */
export async function generateBindings(
  input: ResolvedConfig,
  options: { signal?: AbortSignal } = {},
): Promise<GeneratedBindings> {
  const config = resolveConfig(input);
  const { signal } = options;
  const inputs = new Set(config.configDependencies);
  try {
    collectTypeConfigInputs(config, inputs);
    const commonInputs = [
      ...new Set([
        ...inputs,
        config.tsconfig,
        ...(config.configFile === undefined ? [] : [config.configFile]),
      ]),
    ];
    assertOutputPaths(config, inputs);
    cancelled(signal);
    const discovered = await discoverBindingInputs(config, inputs);
    assertOutputPaths(config, inputs);
    // Discover every module graph before any output can overwrite an authored dependency.
    const prepared: PreparedBinding[] = [];
    for (const binding of discovered) {
      // oxlint-disable-next-line no-await-in-loop -- Trusted exporters can have external effects; join each configured command before starting the next.
      prepared.push(await prepareBinding(binding, config, inputs, commonInputs, signal));
    }
    assertOutputPaths(config, inputs);
    const reusable = await reusableDescriptors(prepared, config, signal);
    const outputs: [string, string][] = [];
    for (const binding of prepared) {
      // oxlint-disable-next-line no-await-in-loop -- Generator subprocesses must settle before a later configured generator starts.
      outputs.push(...(await generateOutput(binding, config.root, signal)));
    }
    assertOutputPaths(config, inputs);
    for (const [file, content] of outputs) {
      // oxlint-disable-next-line no-await-in-loop -- On failure or cancellation, no owned file publication may remain in flight.
      await writeChanged(file, content, signal);
    }
    cancelled(signal);
    for (const candidate of reusable) {
      rememberDescriptor(candidate);
    }
    return { files: outputs.map(([file]) => file), inputs: [...inputs].toSorted() };
  } catch (error) {
    if (error instanceof ToolingError) {
      throw error;
    }
    if (signal?.aborted === true) {
      throw new ToolingError(
        "cancelled",
        "aborted",
        "Binding generation was cancelled",
        undefined,
        { cause: error },
      );
    }
    throw new ToolingError(
      "codegen",
      "generation_failed",
      "Unable to generate contract bindings",
      undefined,
      { cause: error },
    );
  }
}
