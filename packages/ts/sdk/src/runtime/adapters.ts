import { EnvironmentPhase } from "../bridge/protocol.ts";
import { refuse } from "../error.ts";
import { Errors } from "../errors.ts";
import type { Declaration } from "../authoring/declaration.ts";
import type { OrbDefinition, ServiceDefinition } from "../authoring/types.ts";
import type { Environment } from "./records.ts";

// These hooks connect execution backends to the existing lifecycle; they grant no guest authority.
export const declareFactory = Symbol("tembra.declare-factory");
export const validateEnvironment = Symbol("tembra.validate-environment");
export const materialize = Symbol("tembra.materialize");
export const disposeOrb = Symbol("tembra.dispose-orb");
export const releaseEnvironment = Symbol("tembra.release-environment");
export const resourceRealm = Symbol("tembra.resource-realm");
export const admitFactory = Symbol("tembra.admit-factory");
export type AdaptedOrb = OrbDefinition & {
  [declareFactory]?: (declaration: Declaration) => Promise<unknown>;
  [admitFactory]?: () => Promise<() => void>;
  [materialize]?: () => Promise<OrbDefinition>;
  [disposeOrb]?: () => void | Promise<void>;
  [validateEnvironment]?: (environment: Environment, value: unknown) => Promise<void>;
  [releaseEnvironment]?: (environment: Environment) => void | Promise<void>;
};
export type AdaptedService = ServiceDefinition & {
  [declareFactory]?: (declaration: Declaration) => Promise<unknown>;
  [admitFactory]?: () => Promise<() => void>;
};

/** Materialize a fresh revision per installation, disposing earlier candidates on failure. */
export function prepareDefinitions(
  definitions: readonly OrbDefinition[],
): OrbDefinition[] | Promise<OrbDefinition[]> {
  if (!definitions.some((definition) => (definition as AdaptedOrb)[materialize]))
    return [...definitions];
  return prepareBackends(definitions);
}

/** Prepare only definitions that actually need asynchronous backend materialization. */
async function prepareBackends(definitions: readonly OrbDefinition[]): Promise<OrbDefinition[]> {
  const prepared: OrbDefinition[] = [];
  try {
    for (const definition of definitions)
      prepared.push((await (definition as AdaptedOrb)[materialize]?.()) ?? definition);
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
