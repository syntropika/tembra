import type { ContextHandle, ContractReference, ReferenceOf } from "../binding.js";
import type { AnyContract, InputOf, NativeSchema } from "../contracts.js";
import type {
  AcquisitionOptions,
  EsmArtifactV1,
  FacetDefinition,
  ManagedContext,
  Registry,
  RegistryView,
  ReloadOptions,
  ReloadReport,
} from "../executor.js";
import type { AnyHookDefinition } from "../hooks.js";
import { canonicalJson } from "../portable.js";
import type { ObservationBatch } from "./observability.js";

export type {
  ObservationOptions,
  ObservationOperation,
  Observation,
  ObservationBatch,
  ObservationStatistics,
} from "./observability.js";

function snapshotArtifact(artifact: EsmArtifactV1): EsmArtifactV1 {
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Only serialization occurs here; the platform validates the captured artifact.
  return JSON.parse(canonicalJson(artifact)) as EsmArtifactV1;
}

/** A prepared dynamic facet and, optionally, its named ESM export. */
export type ArtifactFacet =
  | EsmArtifactV1
  | {
      readonly artifact: EsmArtifactV1;
      readonly selected?: string;
    };

/** A platform module reference. Relative Node paths use moduleBase; browser URLs use baseURL. */
export type ModuleFacet =
  | string
  | URL
  | { readonly module: string | URL; readonly selected?: string };
export type LoadableFacet = EsmArtifactV1 | string | URL;
export type DynamicFacet = ArtifactFacet | ModuleFacet | FacetDefinition;
export type FacetInput =
  | { readonly kind: "artifact"; readonly artifact: EsmArtifactV1; readonly selected: string }
  | { readonly kind: "module"; readonly module: string; readonly selected: string }
  | { readonly kind: "definition"; readonly definition: FacetDefinition };

/** Captures portable inputs before asynchronous preparation; executable callbacks stay in their realm. */
export function facetInputs(facets: readonly DynamicFacet[]): readonly FacetInput[] {
  return facets.map((facet): FacetInput => {
    if (typeof facet === "string" || facet instanceof URL) {
      return { kind: "module", module: String(facet), selected: "default" };
    }
    if ("artifact" in facet || "format" in facet) {
      const selected = "artifact" in facet ? (facet.selected ?? "default") : "default";
      const artifact = "artifact" in facet ? facet.artifact : facet;
      // Artifacts retain their explicit integrity contract even when malformed extra fields exist.
      return { kind: "artifact", artifact: snapshotArtifact(artifact), selected };
    }
    if ("module" in facet) {
      return {
        kind: "module",
        module: String(facet.module),
        selected: facet.selected ?? "default",
      };
    }
    if ("setup" in facet) {
      return { kind: "definition", definition: facet };
    }
    // The portable serializer rejects accessors and copies the complete artifact before evaluation.
    return { kind: "artifact", artifact: snapshotArtifact(facet), selected: "default" };
  });
}

/** One real Rust Host. Each acquired reference must be released by its consumer. */
/* oxlint-disable typescript/method-signature-style -- Public methods preserve generic contract variance across implementations. */
export type TembraHost = {
  /** Enables generic adapters for this portable hook before installing contributions. */
  registerHook(definition: AnyHookDefinition): void;
  /** Enables resource-backed JavaScript capabilities for one exact nominal identity. */
  registerOpaque<T>(schema: NativeSchema<T>): void;
  install(facet: FacetDefinition): Promise<void>;
  load(facet: LoadableFacet, selectedExport?: string): Promise<void>;
  reload(facets: readonly DynamicFacet[], options?: ReloadOptions): Promise<ReloadReport>;
  hold<C extends AnyContract>(
    contract: C,
    input: InputOf<C>,
    options?: AcquisitionOptions,
  ): ReferenceOf<C>;
  /** Uses hold and readiness; a failed acquisition releases its claim. */
  get<C extends AnyContract>(
    contract: C,
    input: InputOf<C>,
    options?: AcquisitionOptions,
  ): Promise<ReferenceOf<C>>;
  root<T>(operation: (context: ManagedContext) => T | PromiseLike<T>): Promise<T>;
  registry<T>(definition: Registry<T>): RegistryView<T>;
  cancel(context: ContextHandle): void;
  diagnostics(): Readonly<Record<string, unknown>>;
  /** Drains at most limit events; null means collection was disabled at construction. */
  observations(limit: number): ObservationBatch | null;
  /** Joins managed cleanup. Cancellation alone does not prove physical callback completion. */
  close(): Promise<void>;
};
/* oxlint-enable typescript/method-signature-style */

/** Shared acquisition utility; ownership stays in the issuing Rust backend. */
export async function readyReference<C extends AnyContract>(
  reference: ReferenceOf<C>,
): Promise<ReferenceOf<C>> {
  // Every contract reference includes this lifecycle boundary; conditional member inference is erased here.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Every ReferenceOf branch extends this contract lifecycle.
  const lifecycle = reference as ContractReference;
  try {
    await lifecycle.ready;
    return reference;
  } catch (error) {
    lifecycle.release();
    throw error;
  }
}
