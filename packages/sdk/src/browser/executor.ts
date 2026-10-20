import { executorFailure } from "../host/error.js";
import { setupShim, signals, checkedFacet, record, detached } from "../host/executor.js";
import type { WasmExecutorPort, ExecutorWasmModule, RawCandidate } from "../host/executor.js";
import type { FacetInput } from "../host/index.js";
import { facetFromModule } from "../host/loader.js";
import { canonicalJson } from "../index.js";
import type { EsmArtifactV1, FacetDefinition, ReloadOptions, ReloadReport } from "../index.js";
import type { WasmHost } from "./abi.js";
import { evaluateEsmArtifact, realmDiagnostics } from "./artifact.js";
import { browserModuleLoader } from "./module.js";

const freezeObject = Object.freeze;
async function importedFacet(
  input: Extract<FacetInput, { kind: "module" }>,
  base: string,
): Promise<FacetDefinition> {
  const { namespace } = await browserModuleLoader.evaluate({ specifier: input.module, base });
  return facetFromModule(namespace, input.selected);
}

/**
 * Executes trusted facet code against a platform's existing Rust Host. Construct it with the
 * initialized module for that same Host. The caller closes the client, lets outstanding callbacks
 * settle, then frees executor/client raw wrappers; a new executor does not reset the realm budget.
 * Code loading is not a sandbox: evaluation and callbacks can perform irreversible external effects.
 */
export class BrowserExecutor {
  /** Low-level owned executor wrapper; diagnostics track physical work separately from logical cancellation. */
  readonly rust: WasmExecutorPort;
  constructor(host: WasmHost, module: ExecutorWasmModule) {
    this.rust = module.createCheckedWasmExecutor(host);
  }
  /**
   * Installs trusted, already evaluated code and awaits eager Host-lifetime activation. This makes
   * no source/hash/closed-import claim. Initial installation is not a reload transaction: setup or
   * activation failure may leave earlier declarations/effects, so inspect the original rejection
   * and close the Host if the partial initial state is unusable.
   */
  async install(facet: FacetDefinition): Promise<void> {
    await this.rust.install(
      facet.name,
      canonicalJson(facet.uses.map((token) => token.descriptor)),
      setupShim(facet),
    );
  }
  /** Ordinary trusted modules enter the same callback bridge as already imported facets. */
  async loadInput(input: FacetInput, base: string, checkOpen: () => void): Promise<void> {
    if (input.kind === "artifact") {
      await this.load(input.artifact, input.selected);
      return;
    }
    const facet =
      input.kind === "definition"
        ? checkedFacet(record(input.definition))
        : await importedFacet(input, base);
    checkOpen();
    await this.install(facet);
  }

  async reloadInputs(
    inputs: readonly FacetInput[],
    base: string,
    checkOpen: () => void,
    options: ReloadOptions = {},
  ): Promise<ReloadReport> {
    const candidates: RawCandidate[] = [];
    try {
      for (const input of inputs) {
        checkOpen();
        if (input.kind === "artifact") {
          // oxlint-disable-next-line no-await-in-loop -- Each admitted module is physically joined.
          const evaluated = await evaluateEsmArtifact(input.artifact, input.selected, (bytes) =>
            this.rust.reserveGeneration(bytes),
          );
          try {
            checkOpen();
            const facet = checkedFacet(record(evaluated.definition));
            candidates.push(
              evaluated.generation.prepare(
                facet.name,
                canonicalJson(facet.uses.map((token) => token.descriptor)),
                setupShim(facet),
              ),
            );
          } finally {
            evaluated.release();
          }
        } else {
          const facet =
            input.kind === "definition"
              ? checkedFacet(record(input.definition))
              : // oxlint-disable-next-line no-await-in-loop -- Each native import is physically joined before the core transaction.
                await importedFacet(input, base);
          checkOpen();
          candidates.push(
            this.rust.prepare(
              facet.name,
              canonicalJson(facet.uses.map((token) => token.descriptor)),
              setupShim(facet),
            ),
          );
        }
      }
      checkOpen();
      return await this.submit(candidates, options);
    } finally {
      for (const candidate of candidates) {
        candidate.free();
      }
    }
  }
  /**
   * Copies and validates a prebuilt artifact, evaluates it, then installs selected (default "default").
   * Metadata, hash, closed imports and export checks precede evaluation; the artifact limit is
   * 1 MiB of UTF-8. After admission, failed evaluation/installation still consumes budget and may
   * retain module effects. Each document realm permits 32 evaluations and 8 MiB total source;
   * rotation_required requires a new realm, not another Host. Blob URL cleanup does not evict ESM.
   * Rejections preserve loader/core code and reason where supplied; this is trusted code execution.
   */
  private async load(artifact: EsmArtifactV1, selected = "default"): Promise<void> {
    const { generation, definition, release } = await evaluateEsmArtifact(
      artifact,
      selected,
      (bytes) => this.rust.reserveGeneration(bytes),
    );
    try {
      const typed = checkedFacet(record(definition));
      await generation.install(
        typed.name,
        canonicalJson(typed.uses.map((token) => token.descriptor)),
        setupShim(typed),
      );
    } finally {
      release();
    }
  }
  private async submit(
    candidates: readonly RawCandidate[],
    options: ReloadOptions,
  ): Promise<ReloadReport> {
    if (
      Object.keys(options).some(
        (key) =>
          !["remove", "gateDeadline", "retireDeadline", "taskStopDeadline", "signal"].includes(key),
      )
    ) {
      throw executorFailure("reload_options", "unknown reload option");
    }
    const { signal, ...flags } = options;
    const builder = this.rust.beginReload(canonicalJson(flags));
    let result: Promise<string>;
    try {
      for (const candidate of candidates) {
        builder.add(candidate);
      }
      if (signal !== undefined) {
        const raw = signals.get(signal);
        if (!raw) {
          throw executorFailure("reload_signal", "signal was not issued by this binding");
        }
        builder.signal(raw);
      }
      result = builder.start();
    } finally {
      builder.free();
    }
    const report = detached<
      Omit<ReloadReport, "previousVersion"> & { previousVersion: number | null }
    >(await result);
    return freezeObject({ ...report, previousVersion: report.previousVersion ?? undefined });
  }
  /**
   * Returns a detached frozen observation of Rust jobs/queues/generations and realm admissions.
   * Counters grant no mutation authority; jobs may remain after logical cancellation until physical
   * callbacks settle. Use them to observe cleanup, not to infer that external side effects rolled back.
   */
  diagnostics(): Readonly<Record<string, number>> & {
    readonly jobs: number;
    readonly callbacks: number;
    readonly queued: number;
    readonly liveGenerations: number;
  } {
    return freezeObject({
      // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
      ...(JSON.parse(this.rust.diagnosticsJson()) as Record<string, number> & {
        jobs: number;
        callbacks: number;
        queued: number;
        liveGenerations: number;
      }),
      ...realmDiagnostics(),
    });
  }
}
