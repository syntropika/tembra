import { refuse } from "../error.ts";
import { Errors } from "../errors.ts";
import { serviceIdentities, type ServiceIdentity } from "./service-identities.ts";
import type { AcquisitionOptions, ServiceDefinition } from "./types.ts";

/** Associate original and captured services only after the whole orb declaration succeeds. */
export function associateServices(
  orb: string,
  apiVersion: number,
  services: readonly (readonly [string, ServiceDefinition, ServiceDefinition])[],
): void {
  const pending = new Map<ServiceDefinition, ServiceIdentity>();
  for (const [name, original, captured] of services) {
    const identity = { orb, name, apiVersion };
    for (const definition of [original, captured]) {
      const previous = pending.get(definition) ?? serviceIdentities.get(definition);
      if (
        previous &&
        (previous.orb !== orb || previous.name !== name || previous.apiVersion !== apiVersion)
      )
        refuse(
          Errors.Invalid.Service,
          "A service definition must have one orb, name and API version",
        );
      pending.set(definition, identity);
    }
  }
  for (const [definition, identity] of pending) serviceIdentities.set(definition, identity);
}

/** Resolve a declared service without changing the kernel's named acquisition or authority rules. */
export function serviceAcquisition(
  service: string | ServiceDefinition,
  options: AcquisitionOptions = {},
): { name: string; options: AcquisitionOptions } {
  if (typeof service === "string") return { name: service, options };
  const identity = serviceIdentities.get(service);
  if (!identity)
    refuse(Errors.Invalid.Service, "Declare this service in an orb before acquiring it");
  if (options.apiVersion !== undefined && options.apiVersion !== identity.apiVersion)
    refuse(
      Errors.Invalid.Service,
      "The acquisition API version must match the service declaration",
    );
  return { name: identity.name, options: { ...options, apiVersion: identity.apiVersion } };
}
