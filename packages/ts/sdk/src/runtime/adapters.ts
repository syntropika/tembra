import { EnvironmentPhase } from "../bridge/protocol.ts";
import { refuse } from "../error.ts";
import { Errors } from "../errors.ts";
import type { Declaration } from "../authoring/declaration.ts";
import type {
  HostOptions,
  OrbDefinition,
  OrbInput,
  OrbArtifact,
  ServiceDefinition,
} from "../authoring/types.ts";
import type { Environment } from "./records.ts";

// These hooks connect execution backends to the existing lifecycle; they grant no guest authority.
export const declareFactory = Symbol("tembra.declare-factory");
export const validateEnvironment = Symbol("tembra.validate-environment");
export const materialize = Symbol("tembra.materialize");
export const isolate = Symbol("tembra.isolate");
export const signature = Symbol("tembra.signature");
export const signatures = Symbol("tembra.signatures");
export type AdaptedHostOptions = HostOptions & {
  [isolate]?: (definition: OrbArtifact) => Promise<OrbDefinition>;
};
export const disposeOrb = Symbol("tembra.dispose-orb");
export const releaseEnvironment = Symbol("tembra.release-environment");
// Executor-backed revisions; the kernel derives a private resource realm from each revision.
export const isolatedRevision = Symbol("tembra.isolated-revision");
export const admitFactory = Symbol("tembra.admit-factory");
export type AdaptedOrb = OrbDefinition & {
  [isolatedRevision]?: true;
  [declareFactory]?: (declaration: Declaration) => Promise<unknown>;
  [admitFactory]?: () => Promise<() => void>;
  [materialize]?: () => Promise<OrbDefinition>;
  [disposeOrb]?: () => void | Promise<void>;
  [validateEnvironment]?: (environment: Environment, value: unknown) => Promise<void>;
  [releaseEnvironment]?: (environment: Environment) => void | Promise<void>;
};
export type AdaptedService = ServiceDefinition & {
  [signature]?: string;
  [declareFactory]?: (declaration: Declaration) => Promise<unknown>;
  [admitFactory]?: () => Promise<() => void>;
};

/** Materialize a fresh revision per installation, disposing earlier candidates on failure. */
export function prepareDefinitions(
  definitions: readonly OrbInput[],
  isolated?: (definition: OrbArtifact) => Promise<OrbDefinition>,
): OrbDefinition[] | Promise<OrbDefinition[]> {
  if (
    !definitions.some(
      (definition) => "artifact" in definition || (definition as AdaptedOrb)[materialize],
    )
  )
    return [...definitions] as OrbDefinition[];
  const captured = definitions.map((definition) =>
    "artifact" in definition
      ? {
          ...(definition.requires ? { requires: structuredClone(definition.requires) } : {}),
          ...(definition.grants ? { grants: structuredClone(definition.grants) } : {}),
          artifact: definition.artifact.slice(),
        }
      : definition,
  );
  return prepareBackends(captured, isolated);
}

/** Prepare only definitions that actually need asynchronous backend materialization. */
async function prepareBackends(
  definitions: readonly OrbInput[],
  isolated?: (definition: OrbArtifact) => Promise<OrbDefinition>,
): Promise<OrbDefinition[]> {
  const prepared: OrbDefinition[] = [];
  try {
    for (const definition of definitions) {
      if ("artifact" in definition) {
        if (!isolated) throw new Error("This host has no isolated execution backend");
        prepared.push(await isolated(definition));
      } else prepared.push((await (definition as AdaptedOrb)[materialize]?.()) ?? definition);
    }
    return prepared;
  } catch (error) {
    await disposeDefinitions(prepared);
    throw error;
  }
}

/** Release backend revisions only after their physically owning scopes are gone. */
export async function disposeDefinitions(definitions: readonly OrbDefinition[]): Promise<void> {
  const results = await Promise.allSettled(
    definitions.map(async (definition) => (definition as AdaptedOrb)[disposeOrb]?.()),
  );
  const failures = results.filter((result) => result.status === "rejected");
  if (failures.length)
    throw new AggregateError(
      failures.map((result) => result.reason),
      "Backend disposal failed",
    );
}

/** Keep source factories synchronous while a backend prepares their transport asynchronously. */
export function captureEnvironment(
  definition: OrbDefinition | ServiceDefinition,
  declaration: import("../authoring/declaration.ts").Declaration,
): import("./records.ts").Environment | Promise<import("./records.ts").Environment> {
  const environment = declaration.environment;
  const finish = () => {
    environment.phase = EnvironmentPhase.Prepared;
    declaration.session.programs.register(environment);
  };
  const prepare = (definition as AdaptedService)[declareFactory];
  if (prepare)
    return Promise.resolve()
      .then(() => prepare(declaration))
      .then((value) => {
        environment.value = value ?? {};
        return environment;
      })
      .finally(finish);
  try {
    const value = definition.create?.(declaration) ?? {};
    if (value && typeof (value as { then?: unknown }).then === "function")
      refuse(Errors.Invalid.AsyncFactory);
    environment.value = value;
    return environment;
  } finally {
    finish();
  }
}
