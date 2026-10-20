import type { EsmArtifactV1 } from "../executor.js";
import { captureEsmArtifact, inspectEsmArtifact } from "./artifact.js";

/** A checked execution contract, separate from an evaluator implementation and core ownership. */
export type ValidatedArtifactModule = {
  readonly profile: "tembra-esm-v1";
  readonly artifact: EsmArtifactV1;
  readonly sourceBytes: number;
};

/** Private platform seam. Evaluation returns exports, never a second object or lifecycle engine. */
export type ArtifactEvaluator = {
  readonly name: string;
  readonly profile: ValidatedArtifactModule["profile"];
  readonly evaluate: (module: ValidatedArtifactModule, identity: string) => Promise<unknown>;
};

export async function validateArtifactModule(
  artifact: EsmArtifactV1,
  selected: string,
): Promise<ValidatedArtifactModule> {
  const captured = captureEsmArtifact(artifact);
  const sourceBytes = await inspectEsmArtifact(captured, selected);
  return Object.freeze({ profile: "tembra-esm-v1", artifact: captured, sourceBytes });
}
