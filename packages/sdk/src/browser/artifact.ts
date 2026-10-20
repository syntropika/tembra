import type { EsmArtifactV1 } from "../executor.js";
import { assertEsmPrimordials, captureEsmArtifact, inspectEsmArtifact } from "../host/artifact.js";
import { executorFailure } from "../host/error.js";

export { inspectEsmSource, validateEsmArtifact } from "../host/artifact.js";

const createURL = URL.createObjectURL.bind(URL);
const revokeURL = URL.revokeObjectURL.bind(URL);
const ModuleBlob = Blob;
const { apply } = Reflect;
const realm = { evaluations: 0, sourceBytes: 0 };

/** Detached diagnostics; a new Host/executor in this realm cannot reset ESM admission. */
export function realmDiagnostics(): Readonly<{
  realmEvaluations: number;
  realmSourceBytes: number;
}> {
  return Object.freeze({
    realmEvaluations: realm.evaluations,
    realmSourceBytes: realm.sourceBytes,
  });
}

/** Failed admitted evaluation still consumes realm and Rust-session budgets. No cache eviction is claimed. */
export async function evaluateEsmArtifact<T extends { free: () => void }>(
  input: EsmArtifactV1,
  selected: string,
  reserve: (bytes: number) => T,
): Promise<{ readonly generation: T; readonly definition: unknown; readonly release: () => void }> {
  const artifact = captureEsmArtifact(input);
  const bytes = await inspectEsmArtifact(artifact, selected);
  assertEsmPrimordials();
  if (realm.evaluations >= 32 || realm.sourceBytes + bytes > 8_388_608) {
    throw executorFailure(
      "rotation_required",
      "document execution budget exhausted; a new realm is required",
      "unavailable",
    );
  }
  const generation = reserve(bytes);
  const { free } = generation;
  let released = false;
  const release = () => {
    if (!released) {
      released = true;
      apply(free, generation, []);
    }
  };
  try {
    assertEsmPrimordials();
  } catch (error) {
    release();
    throw error;
  }
  realm.evaluations += 1;
  realm.sourceBytes += bytes;
  let url: string | undefined;
  try {
    url = createURL(new ModuleBlob([artifact.source], { type: "text/javascript" }));
    const namespace: unknown = await import(url);
    // SAFETY: the preceding scalar, metadata and closed-source checks establish this artifact shape.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
    return { generation, definition: (namespace as Record<string, unknown>)[selected], release };
  } catch (error) {
    release();
    throw executorFailure(
      "artifact_evaluation",
      error instanceof Error ? error.message : "module evaluation failed",
      "unavailable",
    );
  } finally {
    if (url !== undefined) {
      revokeURL(url);
    }
  }
}
