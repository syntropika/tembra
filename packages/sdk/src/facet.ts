import { Errors } from "./errors.ts";

import { refuse } from "./error.ts";
import type { FacetDefinition, ServiceDefinition } from "./types.ts";

/** Captures executable declarations. No structural schemas or generated contracts are required. */
export function facet<const F extends FacetDefinition>(definition: F): Readonly<F> {
  if (!definition.name || !Number.isInteger(definition.apiVersion) || definition.apiVersion <= 0)
    refuse(Errors.Invalid.Facet, "A facet needs a name and positive API version");
  const services = Object.fromEntries(
    Object.entries(definition.services ?? {}).map(([name, service]) => {
      if (!name || !service.methods || typeof service.methods !== "object")
        refuse(Errors.Invalid.Service);
      if (service.create && typeof service.create !== "function") refuse(Errors.Invalid.Factory);
      for (const handler of Object.values(service.methods)) {
        if (typeof handler !== "function")
          refuse(Errors.Invalid.Method, "Methods must be functions");
      }
      const copy: ServiceDefinition = {
        ...service,
        methods: Object.freeze({ ...service.methods }),
      };
      return [name, Object.freeze(copy)];
    }),
  );
  const requires = Object.fromEntries(
    Object.entries(definition.requires ?? {}).map(([name, requirement]) => [
      name,
      Object.freeze({
        ...requirement,
        ...(requirement.methods ? { methods: Object.freeze([...requirement.methods]) } : {}),
        ...(requirement.hooks ? { hooks: Object.freeze([...requirement.hooks]) } : {}),
        ...(requirement.services
          ? {
              services: Object.freeze(
                Object.fromEntries(
                  Object.entries(requirement.services).map(([name, methods]) => [
                    name,
                    Object.freeze([...methods]),
                  ]),
                ),
              ),
            }
          : {}),
      }),
    ]),
  );
  const hooks = Object.fromEntries(
    Object.entries(definition.hooks ?? {}).map(([name, hook]) => {
      if (typeof hook.run !== "function") refuse(Errors.Invalid.Hook, "Hooks need a run function");
      return [
        name,
        Object.freeze({ ...hook, capabilities: Object.freeze([...(hook.capabilities ?? [])]) }),
      ];
    }),
  );
  const contributes = Object.fromEntries(
    Object.entries(definition.contributes ?? {}).map(([alias, hooks]) => [
      alias,
      Object.freeze(
        Object.fromEntries(
          Object.entries(hooks).map(([name, contribution]) => [
            name,
            typeof contribution === "function" ? contribution : Object.freeze({ ...contribution }),
          ]),
        ),
      ),
    ]),
  );
  return Object.freeze({
    ...definition,
    services: Object.freeze(services),
    requires: Object.freeze(requires),
    hooks: Object.freeze(hooks),
    contributes: Object.freeze(contributes),
  }) as Readonly<F>;
}
