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
  HookDefinition,
} from "../authoring/types.ts";
import type { Environment, ReferenceRecord } from "./records.ts";
import type { Session } from "./session.ts";

import type { Handler } from "../authoring/types.ts";
import type { Context } from "./context.ts";
import type { Encoded } from "../interpreter/codec.ts";
/** Internal platform-neutral resource transport; implementations keep their issuer state private. */
export interface ResourceCapability {
  readonly kind: "resource";
  readonly nominal: { readonly interface: string; readonly name: string };
  readonly reference: ReferenceRecord;
  anchor(session: Session, reference: ReferenceRecord): void;
  lend(): () => void;
  redeem(issuer: object, environment: number, expected: ResourceCapability["nominal"]): object;
  whenClosed(operation: () => void): void;
  close(): void;
}

/** Internal byte-source authority; concrete browser state never enters shared runtime types. */
export interface ByteStreamCapability {
  readonly kind: "bytes";
  readonly finished: Promise<void>;
  /** Share one host notification across publication, receiver reads and abandonment. */
  reportFailure(error: unknown, report: (error: unknown) => void): void;
  bind(context: Context): void;
  close(): void;
}

/** Private capability lane, available only between matched prepared handlers. */
export interface CapturedDelivery {
  readonly value: Encoded;
  readonly capabilities?: readonly (ResourceCapability | ByteStreamCapability)[];
  /** Bounded provider outcome after an early result head; actual work uses the retain sink. */
  readonly finished?: Promise<void>;
}

/** A private data path, never passed to ordinary application handlers or stored on Context. */
export interface CapturedInvocation {
  readonly signature: string;
  readonly input: Encoded;
  output?: Encoded;
  receiver?: ResourceCapability;
  capabilities?: readonly (ResourceCapability | ByteStreamCapability)[];
}
/** The ordinary checked data entry of a prepared backend, with physical retention. */
type CapturedDataMethod = (
  environment: any,
  context: Context,
  args: readonly unknown[],
  retain: (physical: Promise<void>) => void,
) => Promise<unknown>;
interface CapturedMethod {
  readonly signature: string | undefined;
  readonly invokeData?: CapturedDataMethod;
  invoke(
    environment: any,
    context: Context,
    input: Encoded,
    receiver?: ResourceCapability,
    /** Register actual physical completion before the bounded result may resolve or reject. */
    retain?: (physical: Promise<void>) => void,
  ): Promise<Encoded | CapturedDelivery>;
}
const capturedMethods = new WeakMap<Handler, CapturedMethod>();

/** Register the actual prepared handler, rather than certifying a caller-owned object or frame. */
export function bindCaptured(
  handle: Handler,
  signature: string | undefined,
  invoke: CapturedMethod["invoke"],
  invokeData?: CapturedDataMethod,
): Handler {
  if (signature || invokeData)
    capturedMethods.set(handle, { signature, invoke, ...(invokeData ? { invokeData } : {}) });
  return handle;
}

/** Resolve only the implementation selected by admission and only for an identical contract. */
export function capturedMethod(handle: Handler, signature: string): CapturedMethod | undefined {
  const adapter = capturedMethods.get(handle);
  return adapter?.signature === signature ? adapter : undefined;
}

/** Ordinary data callers retain backend work without receiving the capability channel. */
export function capturedDataMethod(handle: Handler): CapturedDataMethod | undefined {
  return capturedMethods.get(handle)?.invokeData;
}

// These hooks connect execution backends to the existing lifecycle; they grant no guest authority.
export const declareFactory = Symbol("tembra.declare-factory");
export const validateEnvironment = Symbol("tembra.validate-environment");
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
  [disposeOrb]?: () => void | Promise<void>;
  [validateEnvironment]?: (environment: Environment, value: unknown) => Promise<void>;
  [releaseEnvironment]?: (environment: Environment) => void | Promise<void>;
};
export type AdaptedService = ServiceDefinition & {
  [signature]?: string;
  [declareFactory]?: (declaration: Declaration) => Promise<unknown>;
  [admitFactory]?: () => Promise<() => void>;
};
export type AdaptedHook = HookDefinition & { [signature]?: string };

/** Materialize a fresh revision per installation, disposing earlier candidates on failure. */
export function prepareDefinitions(
  definitions: readonly OrbInput[],
  isolated?: (definition: OrbArtifact) => Promise<OrbDefinition>,
): OrbDefinition[] | Promise<OrbDefinition[]> {
  if (!definitions.some((definition) => "artifact" in definition))
    return [...definitions] as OrbDefinition[];
  const captured = definitions.map((definition) =>
    "artifact" in definition
      ? {
          ...(definition.requires
            ? {
                requires: Object.fromEntries(
                  Object.entries(definition.requires).map(([name, provider]) => [
                    name,
                    { ...provider },
                  ]),
                ),
              }
            : {}),
          ...(definition.grants
            ? {
                grants: {
                  ...definition.grants,
                  ...(definition.grants.system ? { system: [...definition.grants.system] } : {}),
                  ...(definition.grants.filesystem
                    ? {
                        filesystem: definition.grants.filesystem.map((mount) => ({ ...mount })),
                      }
                    : {}),
                  ...(definition.grants.http
                    ? {
                        http: definition.grants.http.map((grant) => ({
                          ...grant,
                          origins: [...grant.origins],
                        })),
                      }
                    : {}),
                  ...(definition.grants.network
                    ? {
                        network: definition.grants.network.map((grant) => ({
                          ...grant,
                          ...(grant.connect ? { connect: [...grant.connect] } : {}),
                          ...(grant.listen ? { listen: [...grant.listen] } : {}),
                          ...(grant.bind ? { bind: [...grant.bind] } : {}),
                          ...(grant.datagrams ? { datagrams: [...grant.datagrams] } : {}),
                        })),
                      }
                    : {}),
                },
              }
            : {}),
          artifact: definition.artifact.slice(),
          ...(definition.cli ? { cli: captureCli(definition.cli) } : {}),
        }
      : definition,
  );
  return prepareBackends(captured, isolated);
}

/** Overlap independent native preparations while bounding transient Stores and draining failures. */
async function prepareBackends(
  definitions: (OrbInput | undefined)[],
  isolated?: (definition: OrbArtifact) => Promise<OrbDefinition>,
): Promise<OrbDefinition[]> {
  const prepared: (OrbDefinition | undefined)[] = new Array(definitions.length);
  let next = 0;
  const failures: { index: number; reason: unknown }[] = [];
  async function prepare(): Promise<void> {
    while (failures.length === 0 && next < definitions.length) {
      const index = next++;
      const definition = definitions[index]!;
      try {
        if ("artifact" in definition) {
          if (!isolated) throw new Error("This host has no isolated execution backend");
          const materialized = await isolated(definition);
          if (!materialized) throw new Error("Execution backend returned no Orb definition");
          prepared[index] = materialized;
        } else prepared[index] = definition;
      } catch (error) {
        failures.push({ index, reason: error });
      } finally {
        // Release our captured input once the backend owns the prepared revision.
        definitions[index] = undefined;
      }
    }
  }
  // Workers consume their errors so every in-flight Store settles before disposal begins.
  await Promise.all(Array.from({ length: Math.min(4, definitions.length) }, prepare));
  const completed = prepared.filter((definition) => definition !== undefined);
  if (failures.length) {
    const errors = failures.sort((a, b) => a.index - b.index).map((failure) => failure.reason);
    return discardDefinitions(
      completed,
      errors.length === 1 ? errors[0] : new AggregateError(errors, "Orb preparation failed"),
    );
  }
  return completed;
}

/** Dispose failed candidates while retaining both the operation and cleanup causes. */
export async function discardDefinitions(
  definitions: readonly OrbDefinition[],
  error: unknown,
): Promise<never> {
  try {
    await disposeDefinitions(definitions);
  } catch (disposal) {
    throw new AggregateError([error, disposal], "Orb operation and disposal failed");
  }
  throw error;
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
import { captureCli } from "../interpreter/cli.ts";
