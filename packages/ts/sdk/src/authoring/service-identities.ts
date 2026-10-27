import type { ServiceDefinition } from "./types.ts";

export interface ServiceIdentity {
  readonly orb: string;
  readonly name: string;
  readonly apiVersion: number;
}

// Standalone orb bundles and public SDK entrypoints share metadata within one JavaScript realm.
// Weak keys retain neither removed implementations nor an additional object lifecycle.
const identityKey = Symbol.for("@syntropika/tembra-sdk/service-identities");
const existing: unknown = Object.getOwnPropertyDescriptor(globalThis, identityKey)?.value;
if (existing !== undefined && !(existing instanceof WeakMap))
  throw new TypeError("The SDK service identity slot must contain a WeakMap");

export const serviceIdentities: WeakMap<ServiceDefinition, ServiceIdentity> =
  existing instanceof WeakMap ? existing : new WeakMap();

if (existing === undefined)
  Object.defineProperty(globalThis, identityKey, { value: serviceIdentities });
