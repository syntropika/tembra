import { emitWarning } from "node:process";

import type { FacetDefinition } from "../executor.js";
import { validateArtifactModule } from "../host/artifact-runtime.js";
import type { ArtifactEvaluator, ValidatedArtifactModule } from "../host/artifact-runtime.js";
import { executorFailure } from "../host/error.js";
import { checkedFacet, record, setupShim } from "../host/executor.js";
import type { FacetInput } from "../host/index.js";
import { facetFromModule } from "../host/loader.js";
import { canonicalJson } from "../portable.js";
import type { NativeTransport } from "./bridge.js";
import { captureSource, sourceEvaluator } from "./source.js";
import type { SourceModule } from "./source.js";

export type PreparedFacet = { readonly free: () => void };
type Generation = { readonly free: () => void };
export type Preparation = {
  readonly reserve: (costs: readonly number[]) => void;
  readonly reserveSources: (costs: readonly number[]) => void;
  readonly chargeSourceModule: (bytes: number) => void;
  readonly submitSource: Preparation["submit"];
  readonly submit: (sourceHash: string) => {
    readonly generation: Generation;
    readonly identity: string;
    readonly warning: boolean;
  };
  readonly evaluationFailed: (source?: boolean) => void;
  readonly committing: () => void;
  readonly free: () => void;
};
type Module =
  | {
      readonly kind: "artifact";
      readonly validated: ValidatedArtifactModule;
      readonly selections: string[];
    }
  | { readonly kind: "source"; readonly source: SourceModule; readonly selections: string[] }
  | { readonly kind: "definition"; readonly definition: FacetDefinition };

async function planFacets(
  selections: readonly FacetInput[],
  evaluator: ArtifactEvaluator,
  moduleBase: URL,
): Promise<Map<string, Module>> {
  const modules = new Map<string, Module>();
  for (const item of selections) {
    if (item.kind === "definition") {
      modules.set(`definition:${modules.size}`, {
        kind: "definition",
        definition: checkedFacet(record(item.definition)),
      });
    } else if (item.kind === "module") {
      // oxlint-disable-next-line no-await-in-loop -- Capture the full batch before evaluating any module.
      const source = await captureSource(item.module, moduleBase);
      const key = `source:${source.filename}`;
      const known = modules.get(key);
      if (known?.kind === "source") {
        known.selections.push(item.selected);
      } else {
        modules.set(key, { kind: "source", source, selections: [item.selected] });
      }
    } else {
      // oxlint-disable-next-line no-await-in-loop -- Validate every artifact selection before admission.
      const validated = await validateArtifactModule(item.artifact, item.selected);
      if (evaluator.profile !== validated.profile) {
        throw executorFailure(
          "artifact_profile",
          "evaluator does not implement the artifact execution profile",
        );
      }
      const key = canonicalJson([validated.artifact.identity, validated.artifact.sha256]);
      const known = modules.get(key);
      if (known?.kind === "artifact") {
        known.selections.push(item.selected);
      } else {
        modules.set(key, { kind: "artifact", validated, selections: [item.selected] });
      }
    }
  }
  return modules;
}

/** Physically joins each admitted import; the caller owns preparation until this operation settles. */
export async function withPreparedFacets<T>(
  transport: NativeTransport,
  evaluator: ArtifactEvaluator,
  selections: readonly FacetInput[],
  moduleBase: URL,
  preparation: Preparation,
  checkOpen: () => void,
  commit: (facets: readonly PreparedFacet[]) => Promise<T>,
): Promise<T> {
  if (selections.length > 1024) {
    throw executorFailure("artifact_batch", "at most 1024 facet selections are allowed per batch");
  }
  const modules = await planFacets(selections, evaluator, moduleBase);
  checkOpen();
  preparation.reserve(
    [...modules.values()].flatMap((module) =>
      module.kind === "artifact" ? [module.validated.sourceBytes] : [],
    ),
  );
  preparation.reserveSources(
    [...modules.values()].flatMap((module) =>
      module.kind === "source" ? [module.source.bytes] : [],
    ),
  );
  const sourceLoader = [...modules.values()].some((module) => module.kind === "source")
    ? sourceEvaluator(preparation)
    : undefined;
  const candidates: PreparedFacet[] = [];
  try {
    for (const module of modules.values()) {
      checkOpen();
      if (module.kind === "definition") {
        const facet = module.definition;
        candidates.push(
          transport.request<PreparedFacet>("prepareInstalled", [
            facet.name,
            canonicalJson(facet.uses.map((token) => token.descriptor)),
            setupShim(facet),
          ]),
        );
        continue;
      }
      // Submission is charged permanently before evaluating trusted code.
      const { generation, identity, warning } =
        module.kind === "source"
          ? preparation.submitSource(module.source.hash)
          : preparation.submit(module.validated.artifact.sha256);
      try {
        if (warning) {
          emitWarning(
            `Tembra Node ${module.kind === "source" ? "source" : "ESM artifact"} lifetime budget has reached 75%; plan explicit process rotation`,
            {
              code: "TEMBRA_LOADER_BUDGET",
            },
          );
        }
        let namespace: unknown;
        try {
          if (module.kind === "source") {
            if (!sourceLoader) {
              throw executorFailure("source_loader", "source loader is unavailable");
            }
            // oxlint-disable-next-line no-await-in-loop -- Module acquisition is physically joined before preparation continues.
            const evaluated = await sourceLoader.evaluate(module.source);
            ({ namespace } = evaluated);
          } else {
            // oxlint-disable-next-line no-await-in-loop -- Each physical evaluation is joined before the next module is submitted.
            namespace = await evaluator.evaluate(module.validated, identity);
          }
        } catch (error) {
          preparation.evaluationFailed(module.kind === "source");
          if (module.kind === "source" && error instanceof Error && "reason" in error) {
            throw error;
          }
          throw executorFailure(
            module.kind === "source" ? "module_evaluation" : "artifact_evaluation",
            error instanceof Error ? error.message.slice(0, 1024) : "module evaluation failed",
            "unavailable",
          );
        }
        checkOpen();
        for (const selected of module.selections) {
          const facet = facetFromModule(namespace, selected);
          candidates.push(
            transport.request<PreparedFacet>("prepareFacet", [
              facet.name,
              canonicalJson(facet.uses.map((token) => token.descriptor)),
              setupShim(facet),
              generation,
            ]),
          );
        }
      } finally {
        generation.free();
      }
    }
    checkOpen();
    preparation.committing();
    return await commit(candidates);
  } finally {
    sourceLoader?.close();
    for (const candidate of candidates) {
      candidate.free();
    }
  }
}
