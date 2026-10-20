import type { FacetDefinition } from "../executor.js";
import { checkedFacet, record } from "./executor.js";

/** Platform module acquisition/evaluation only; prepared-facet ownership belongs to its bridge. */
export type FacetModuleLoader<Input> = {
  readonly evaluate: (
    input: Input,
  ) => { readonly namespace: unknown } | PromiseLike<{ readonly namespace: unknown }>;
};

/** Module delivery is independent of artifact formats and Rust execution versions. */
export function facetFromModule(namespace: unknown, selected = "default"): FacetDefinition {
  return checkedFacet(record(record(namespace)[selected]));
}
