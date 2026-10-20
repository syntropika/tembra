import { executorFailure } from "../host/error.js";
import type { FacetModuleLoader } from "../host/loader.js";

type Admission = { evaluations: number; failures: number; active: boolean; started: number };
const key = Symbol.for("@tembra/sdk/browser/module-admission/v1");
const existing: unknown = Reflect.get(globalThis, key);
function isAdmission(value: unknown): value is Admission {
  return (
    value !== null &&
    typeof value === "object" &&
    "evaluations" in value &&
    typeof value.evaluations === "number" &&
    "failures" in value &&
    typeof value.failures === "number" &&
    "active" in value &&
    typeof value.active === "boolean" &&
    "started" in value &&
    typeof value.started === "number"
  );
}
if (existing !== undefined && !isAdmission(existing)) {
  throw executorFailure("loader_authority", "browser module budget authority is invalid");
}
const admission = isAdmission(existing)
  ? existing
  : { evaluations: 0, failures: 0, active: false, started: 0 };
if (existing === undefined) {
  Object.defineProperty(globalThis, key, { value: admission });
}
const limit = 256;

export function moduleDiagnostics() {
  return Object.freeze({
    profile: "browser-module-v1",
    evaluator: "native-esm",
    evaluations: admission.evaluations,
    evaluationFailures: admission.failures,
    maxEvaluations: limit,
    remainingEvaluations: limit - admission.evaluations,
    warning: admission.evaluations >= (limit * 3) / 4,
    requiresRealmRotation: admission.evaluations >= limit,
    inFlight: admission.active,
    physicalUnloadSupported: false,
    elapsedMs: admission.active ? Date.now() - admission.started : null,
    sourceBytes: null,
    transitiveModules: null,
  });
}

export function browserModuleBase(input?: string | URL): string {
  const ambient = typeof document === "undefined" ? globalThis.location?.href : document.baseURI;
  return input === undefined
    ? (ambient ?? import.meta.url)
    : new URL(String(input), ambient ?? import.meta.url).href;
}

/** Native import maps, CORS, CSP and dependency caching remain browser responsibilities. */
async function evaluateBrowserModule(specifier: string, base: string): Promise<unknown> {
  if (admission.active) {
    throw executorFailure(
      "loader_busy",
      "another browser module import is in flight",
      "unavailable",
    );
  }
  if (!specifier) {
    throw executorFailure("module_url", "module URL must not be empty");
  }
  let resolved: string;
  try {
    // Bare names are delegated unchanged to native import-map resolution.
    const bare = !/^(?:\.{1,2}\/|\/|[a-z][a-z0-9+.-]*:)/iu.test(specifier);
    resolved = bare ? specifier : new URL(specifier, base).href;
  } catch {
    throw executorFailure("module_url", "invalid browser module URL");
  }
  if (admission.evaluations >= limit) {
    const error = executorFailure(
      "loader_budget_exhausted",
      "browser module import budget exhausted",
      "unavailable",
    );
    const details = Object.freeze({
      profile: "browser-module-v1",
      dimension: "evaluations",
      used: admission.evaluations,
      requested: 1,
      limit,
    });
    Object.defineProperty(error, "details", { value: details, enumerable: true });
    Object.defineProperty(error, "detailsJson", {
      value: JSON.stringify(details),
      enumerable: true,
    });
    throw error;
  }
  admission.evaluations += 1;
  admission.active = true;
  admission.started = Date.now();
  try {
    const namespace: unknown = await import(resolved);
    return namespace;
  } catch (error) {
    admission.failures += 1;
    throw executorFailure(
      "module_evaluation",
      `could not import ${resolved}: ${error instanceof Error ? error.message : "module import failed"}`,
      "unavailable",
    );
  } finally {
    admission.active = false;
  }
}

export const browserModuleLoader: FacetModuleLoader<{
  readonly specifier: string;
  readonly base: string;
}> = {
  evaluate: async ({ specifier, base }) => ({
    namespace: await evaluateBrowserModule(specifier, base),
  }),
};
