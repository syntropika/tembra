// The compiled pipeline's dependencies as TypeScript Orbs that refuse every call with
// `released/reference`, shared by the Node host and the JS component adapter of one test.
// A structured refusal is recognized by its fields, so any SDK copy's error serves both hosts.
import { Errors, TembraError } from "../dist/index.js";

const services = {
  arithmetic: "example:pipeline/arithmetic@1.0.0",
  delayed: "example:pipeline/delayed@1.0.0",
};

/** The pipeline's requirements by imported interface. */
export const requires = Object.fromEntries(
  Object.entries(services).map(([orb, service]) => [service, { orb, apiVersion: 1 }]),
);

/**
 * The two dependencies, each bound to the WIT digest the pipeline requires of it. `signature` is
 * the symbol of the SDK copy that installs them: Bun resolves the package to its sources, Node to
 * its published modules.
 */
export function dependencies(signatures, signature) {
  return Object.entries(services).map(([orb, service]) => ({
    name: orb,
    apiVersion: 1,
    services: {
      [`${orb}::${service}`]: {
        [signature]: signatures[orb],
        methods: {
          double() {
            throw new TembraError(Errors.Released.Reference);
          },
        },
      },
    },
  }));
}
