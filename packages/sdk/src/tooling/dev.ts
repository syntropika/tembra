import { createHash } from "node:crypto";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

import { HostDeliveryError } from "./adapters.js";
import { build } from "./build.js";
import { loadConfig, resolveConfig } from "./config.js";
import { ToolingError } from "./error.js";
import type {
  BuildResult,
  BuiltArtifact,
  DevelopmentHost,
  DevOptions,
  DevSession,
  Platform,
  ResolvedConfig,
  ToolingConfig,
} from "./types.js";

const temporaryId = "[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
const temporaryName = new RegExp(
  `^(?:\\.tembra-${temporaryId}\\.mjs|\\.stage-${temporaryId})$|\\.tembra-${temporaryId}\\.tmp$`,
  "u",
);

async function fileStamp(file: string): Promise<string> {
  try {
    return createHash("sha256")
      .update(await readFile(file))
      .digest("hex");
  } catch (error) {
    if (error !== null && typeof error === "object" && "code" in error && error.code === "ENOENT") {
      return "missing";
    }
    throw error;
  }
}

function captureHosts(input: DevOptions["hosts"]): Partial<Record<Platform, DevelopmentHost>> {
  const captured = new Map<DevelopmentHost, DevelopmentHost>();
  const hosts: Partial<Record<Platform, DevelopmentHost>> = {};
  for (const platform of ["server", "browser"] as const) {
    const original = input?.[platform];
    if (original === undefined) {
      continue;
    }
    let host = captured.get(original);
    if (host === undefined) {
      host = {
        apply: original.apply.bind(original),
        ...(original.close === undefined ? {} : { close: original.close.bind(original) }),
      };
      captured.set(original, host);
    }
    hosts[platform] = host;
  }
  return hosts;
}
function deliveryKey(artifact: BuiltArtifact): string {
  return JSON.stringify([artifact.platform, artifact.target]);
}
function deliveryStamp(artifact: BuiltArtifact): string {
  return JSON.stringify([
    artifact.artifact.identity,
    artifact.artifact.sha256,
    artifact.selectedExport,
  ]);
}

function isHostRefusal(error: unknown): boolean {
  return (
    error instanceof HostDeliveryError ||
    (error !== null &&
      typeof error === "object" &&
      "code" in error &&
      typeof error.code === "string" &&
      "reason" in error &&
      typeof error.reason === "string")
  );
}
/**
 * Watch project inputs, build affected targets and deliver through explicitly issued host capabilities.
 * Resolves after the initial filesystem scan; `session.ready` waits for the first build attempt,
 * including a failed attempt reported to `onError`. The session owns supplied hosts after validation.
 * Observers run synchronously; returned promises are not joined. Manual rebuild paths resolve against
 * the project root. Configuration edits refresh build settings while retaining the original hosts.
 * Shared generation/configuration failures skip the cycle. Independent target failures retain that
 * target's last artifact while successful siblings can install; reload failures preserve their cause.
 * Newly discovered dependencies receive a verification build before delivery/readiness.
 * Close stops new cycles, requests cancellation, joins local build/delivery work, and closes adapters.
 * Hosts are adopted after configuration validation; initialization failure closes them. Admitted
 * callback references/configuration are captured before yielding. Identical acknowledged artifacts
 * are not reinstalled; pending or failed deliveries remain retryable. Transport cancellation cannot
 * prove remote rollback or physical callback termination.
 *
 * @example
 * ```ts
 * import { once } from "node:events";
 * import { dev, loadConfig } from "@tembra/sdk/tooling";
 * const session = await dev(await loadConfig(), { onError: console.error });
 * try {
 *   await session.ready;
 *   await once(process, "SIGINT");
 * } finally {
 *   await session.close();
 * }
 * ```
 * Supply installation capabilities through `DevOptions.hosts` and explicit embedding adapters.
 */
export async function dev(
  input: ToolingConfig | ResolvedConfig,
  options: DevOptions = {},
): Promise<DevSession> {
  const { signal, onBuild, onReload, onError } = options;
  let config = resolveConfig(input);
  const hosts = captureHosts(options.hosts);
  const controller = new AbortController();
  let closed = false;
  let running: Promise<void> | undefined;
  let polling: Promise<void> | undefined;
  let pollTimer: ReturnType<typeof setTimeout> | undefined;
  let debounceTimer: ReturnType<typeof setTimeout> | undefined;
  let fullBuild = true;
  const changes = new Set<string>();
  const graph = new Map<string, Set<string>>();
  const shared = new Set<string>();
  const generated = new Set<string>();
  const failed = new Set<string>();
  const lastApplied = new Map<string, string>();
  let markReady!: () => void;
  // oxlint-disable-next-line promise/avoid-new -- Readiness is resolved by the first settled development cycle or close.
  const ready = new Promise<void>((resolve) => {
    markReady = resolve;
  });
  const report = (error: unknown) => {
    if (closed) {
      return;
    }
    try {
      onError?.(error instanceof Error ? error : new Error(String(error)));
    } catch {
      /* Observers do not own session lifecycle. */
    }
  };
  const observe = (observer: (() => void) | undefined) => {
    try {
      observer?.();
    } catch (error) {
      report(error);
    }
  };
  const normalize = (file: string) => path.resolve(config.root, file);
  function sharedInputs(): Set<string> {
    const files = new Set([config.tsconfig, ...(config.configDependencies ?? [])]);
    if (config.configFile !== undefined) {
      files.add(config.configFile);
    }
    for (const binding of config.codegen) {
      if ("descriptor" in binding.source) {
        files.add(binding.source.descriptor);
      } else if ("module" in binding.source) {
        files.add(binding.source.module);
      } else {
        for (const file of binding.source.watch) {
          files.add(file);
        }
      }
      for (const file of [binding.descriptor, binding.typescript, binding.rust]) {
        if (file !== undefined) {
          generated.add(normalize(file));
        }
      }
    }
    return new Set([...files, ...shared].map(normalize));
  }
  async function snapshot(): Promise<Map<string, string>> {
    const files = new Set<string>(sharedInputs());
    for (const target of Object.values(config.builds)) {
      files.add(normalize(target.entry));
    }
    for (const inputs of graph.values()) {
      for (const file of inputs) {
        files.add(file);
      }
    }
    const excluded = path.resolve(config.outDir);
    async function walk(directory: string): Promise<void> {
      let entries;
      try {
        entries = await readdir(directory, { withFileTypes: true });
      } catch (error) {
        if (
          error !== null &&
          typeof error === "object" &&
          "code" in error &&
          error.code === "ENOENT"
        ) {
          return;
        }
        throw error;
      }
      for (const entry of entries) {
        if (
          ["node_modules", ".git", "target"].includes(entry.name) ||
          temporaryName.test(entry.name)
        ) {
          continue;
        }
        const file = path.join(directory, entry.name);
        if (file === excluded || generated.has(file)) {
          continue;
        }
        if (entry.isDirectory()) {
          // oxlint-disable-next-line no-await-in-loop -- Sequential recursion bounds open directory work and joins each scan before its parent settles.
          await walk(file);
        } else if (entry.isFile()) {
          files.add(file);
        }
      }
    }
    await walk(config.root);
    const result = new Map<string, string>();
    const reads = await Promise.allSettled(
      [...files]
        .filter((file) => !generated.has(file) && !temporaryName.test(path.basename(file)))
        .map(async (file) => {
          result.set(file, await fileStamp(file));
        }),
    );
    const failure = reads.find((outcome) => outcome.status === "rejected");
    if (failure?.status === "rejected") {
      throw failure.reason;
    }
    return result;
  }
  let previous = new Map<string, string>();
  const watched = new Set<string>();
  let watchRevision = 0;
  async function adoptInputs(inputs: readonly string[]): Promise<boolean> {
    const files = [...new Set(inputs.map(normalize))].filter(
      (file) => (!watched.has(file) || !previous.has(file)) && !generated.has(file),
    );
    const observations = await Promise.allSettled(
      files.map(async (file) => ({ file, stamp: await fileStamp(file) })),
    );
    const failure = observations.find((outcome) => outcome.status === "rejected");
    if (failure?.status === "rejected") {
      throw failure.reason;
    }
    if (files.length) {
      watchRevision += 1;
    }
    for (const observation of observations) {
      if (observation.status === "fulfilled") {
        const { file, stamp } = observation.value;
        // Preserve observations and queued edits made by polling while this build was active.
        if (!previous.has(file)) {
          previous.set(file, stamp);
        }
        watched.add(file);
      }
    }
    return files.length > 0;
  }
  async function compileTargets(targets: readonly string[]): Promise<BuildResult> {
    // New external dependencies need a baseline before graph adoption and a verification read
    // before delivery: otherwise discovery looks like an edit, or a concurrent edit is swallowed.
    let discovered: boolean;
    let result: BuildResult;
    let errors: unknown[];
    do {
      // oxlint-disable-next-line no-await-in-loop -- Generation is shared by this serialized cycle, before independent target compilation.
      const common = await build(config, { targets: [], signal: controller.signal });
      for (const file of common.generated.files) {
        generated.add(normalize(file));
      }
      // oxlint-disable-next-line no-await-in-loop -- Baseline new generation dependencies before exposing their watch graph.
      discovered = await adoptInputs(common.inputs);
      // Configuration and lock inputs must remain visible between shared adoption and target
      // compilation; otherwise polling mistakes their temporary absence for a source deletion.
      for (const file of common.inputs) {
        shared.add(normalize(file));
      }
      const artifacts: BuiltArtifact[] = [];
      const inputs = new Set(common.inputs);
      errors = [];
      for (const target of targets) {
        if (closed) {
          break;
        }
        try {
          // Production build remains atomic; development gives each target its own publication boundary.
          // oxlint-disable-next-line no-await-in-loop -- Join each owned target build, including refusal, before proceeding.
          const built = await build(
            { ...config, codegen: [] },
            { targets: [target], signal: controller.signal },
          );
          // oxlint-disable-next-line no-await-in-loop -- Baseline first discoveries before the graph can be inspected by polling.
          const newInputs = await adoptInputs(built.inputs);
          discovered ||= newInputs;
          for (const artifact of built.artifacts) {
            graph.set(artifact.target, new Set(artifact.inputs.map(normalize)));
            failed.delete(artifact.target);
            artifacts.push(artifact);
          }
          for (const file of built.inputs) {
            inputs.add(file);
          }
        } catch (error) {
          failed.add(target);
          errors.push(error);
        }
      }
      result = { artifacts, generated: common.generated, inputs: [...inputs].toSorted() };
      // oxlint-disable-next-line no-unmodified-loop-condition -- close() changes admission while compilation is awaiting owned work.
    } while (discovered && !closed);
    for (const error of errors) {
      report(error);
    }
    return result;
  }
  async function selectTargets(paths: Set<string>, all: boolean): Promise<string[]> {
    const configInputs = new Set(
      [config.configFile, ...(config.configDependencies ?? [])].filter(
        (file): file is string => file !== undefined,
      ),
    );
    if (config.configFile !== undefined && [...paths].some((file) => configInputs.has(file))) {
      config = await loadConfig(config.configFile);
      watchRevision += 1;
      graph.clear();
      shared.clear();
      generated.clear();
      failed.clear();
      return Object.keys(config.builds);
    } else if (all || [...paths].some((file) => sharedInputs().has(file))) {
      return Object.keys(config.builds);
    }
    return Object.entries(config.builds)
      .filter(
        ([name, target]) =>
          failed.has(name) ||
          paths.has(normalize(target.entry)) ||
          [...paths].some((file) => graph.get(name)?.has(file) === true),
      )
      .map(([name]) => name);
  }
  async function deliver(snapshots: readonly BuiltArtifact[]): Promise<void> {
    for (const platform of ["server", "browser"] as const satisfies readonly Platform[]) {
      if (closed) {
        break;
      }
      const artifacts = snapshots.filter(
        (artifact) =>
          artifact.platform === platform &&
          lastApplied.get(deliveryKey(artifact)) !== deliveryStamp(artifact),
      );
      if (!artifacts.length) {
        continue;
      }
      try {
        const host = hosts[platform];
        const outcome = host
          ? // oxlint-disable-next-line no-await-in-loop -- Each platform delivery settles before another host admits replacement work.
            await host.apply(artifacts, controller.signal)
          : {
              status: "pending" as const,
              message: `No ${platform} development host is connected`,
            };
        for (const artifact of artifacts) {
          if (outcome.status === "applied") {
            lastApplied.set(deliveryKey(artifact), deliveryStamp(artifact));
          } else {
            lastApplied.delete(deliveryKey(artifact));
          }
        }
        if (!closed) {
          observe(() =>
            onReload?.({
              platform,
              targets: artifacts.map((artifact) => artifact.target),
              outcome,
            }),
          );
        }
      } catch (error) {
        for (const artifact of artifacts) {
          lastApplied.delete(deliveryKey(artifact));
        }
        const refused = isHostRefusal(error);
        report(
          new ToolingError(
            "reload",
            refused ? "host_rejected" : "delivery_failed",
            refused
              ? `The ${platform} development host rejected the update`
              : `Delivery to the ${platform} development host failed; its committed outcome may be unknown`,
            undefined,
            { cause: error },
          ),
        );
      }
    }
  }
  const run = async (): Promise<void> => {
    if (closed) {
      await Promise.resolve();
      return;
    }
    if (running) {
      await running;
      return;
    }
    running = (async () => {
      // oxlint-disable-next-line no-unmodified-loop-condition -- close() changes admission while an awaited build or delivery is in flight.
      while (!closed && (fullBuild || changes.size)) {
        const paths = new Set(changes);
        changes.clear();
        const all = fullBuild;
        fullBuild = false;
        let targets: string[] = [];
        try {
          // oxlint-disable-next-line no-await-in-loop -- Configuration refresh is part of this serialized cycle and precedes its build.
          targets = await selectTargets(paths, all);
          if (!targets.length || closed) {
            continue;
          }
          // oxlint-disable-next-line no-await-in-loop -- Each build must finish before its checked artifacts are published and delivered.
          const result = await compileTargets(targets);
          if (!result.artifacts.length || closed) {
            continue;
          }
          const deliveries = result.artifacts.map((artifact) => ({
            ...artifact,
            inputs: [...artifact.inputs],
          }));
          observe(() => onBuild?.(result));
          // oxlint-disable-next-line no-await-in-loop -- Join every authorized delivery before the next development cycle.
          await deliver(deliveries);
        } catch (error) {
          for (const target of targets.length ? targets : Object.keys(config.builds)) {
            failed.add(target);
          }
          report(error);
        } finally {
          markReady();
        }
      }
    })().finally(() => {
      running = undefined;
      markReady();
    });
    await running;
  };
  const schedule = () => {
    if (closed) {
      return;
    }
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      debounceTimer = undefined;
      run().catch(report);
    }, config.dev.debounce ?? 50);
  };
  const poll = () => {
    if (closed) {
      return;
    }
    polling = (async () => {
      try {
        const revision = watchRevision;
        const current = await snapshot();
        // An in-flight scan of an earlier graph cannot remove freshly adopted watch entries.
        if (closed || revision !== watchRevision) {
          return;
        }
        let changed = false;
        for (const file of new Set([...previous.keys(), ...current.keys()])) {
          if (previous.get(file) !== current.get(file)) {
            changes.add(file);
            changed = true;
          }
        }
        previous = current;
        if (changed) {
          schedule();
        }
      } catch (error) {
        report(
          new ToolingError(
            "watch",
            "poll_failed",
            "Cannot inspect development sources",
            undefined,
            { cause: error },
          ),
        );
      }
    })().finally(() => {
      polling = undefined;
      if (!closed) {
        pollTimer = setTimeout(poll, config.dev.pollInterval ?? 100);
      }
    });
  };
  let closing: Promise<void> | undefined;
  const externalAbort = () => {
    void close().catch(report);
  };
  async function close(): Promise<void> {
    await (closing ??= (async () => {
      closed = true;
      controller.abort();
      clearTimeout(pollTimer);
      clearTimeout(debounceTimer);
      signal?.removeEventListener("abort", externalAbort);
      // Host cancellation must settle delivery before resources are torn down.
      await Promise.allSettled([polling, running]);
      const ownedHosts = [...new Set(Object.values(hosts))];
      const outcomes = await Promise.allSettled(
        ownedHosts.map(async (host) => {
          await host.close?.();
        }),
      );
      markReady();
      const failure = outcomes.find((outcome) => outcome.status === "rejected");
      if (failure?.status === "rejected") {
        throw failure.reason;
      }
    })());
  }
  if (signal?.aborted !== true) {
    try {
      previous = await snapshot();
      for (const file of previous.keys()) {
        watched.add(file);
      }
    } catch (error) {
      let cause = error;
      try {
        await close();
      } catch (cleanupError) {
        cause = new AggregateError(
          [error, cleanupError],
          "Development initialization and host cleanup failed",
          { cause: error },
        );
      }
      throw new ToolingError(
        "watch",
        "initial_snapshot_failed",
        "Cannot inspect initial development sources",
        undefined,
        { cause },
      );
    }
  }
  signal?.addEventListener("abort", externalAbort, { once: true });
  if (signal?.aborted === true) {
    await close();
  } else {
    run().catch(report);
    pollTimer = setTimeout(poll, config.dev.pollInterval ?? 100);
  }
  return {
    ready,
    async rebuild(paths) {
      if (closed) {
        throw new ToolingError("cancelled", "session_closed", "The development session is closed");
      }
      if (paths) {
        for (const file of paths) {
          changes.add(normalize(file));
        }
      } else {
        fullBuild = true;
      }
      clearTimeout(debounceTimer);
      debounceTimer = undefined;
      await run();
    },
    close,
  };
}
