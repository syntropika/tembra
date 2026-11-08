import type { NodeComponentPort } from "@syntropika/tembra-engine-node";
import type { OrbDefinition } from "../authoring/types.ts";
import type { Context } from "../runtime/context.ts";
import { signature, disposeOrb, isolatedRevision, type AdaptedOrb } from "#tembra-adapters";
import { answer } from "./types.ts";

/** Compiled callbacks enter the same kernel-owned call lifecycle as interpreted source callbacks. */
export function componentDefinition(
  port: NodeComponentPort,
  name: string,
  apiVersion: number,
): OrbDefinition {
  const plan = JSON.parse(port.description()) as {
    orb: { services: { name: string; methods: string[]; signature?: string }[] };
  };
  return {
    name,
    apiVersion,
    services: Object.fromEntries(
      plan.orb.services.map((service) => [
        service.name,
        {
          [signature]: service.signature,
          methods: Object.fromEntries(
            service.methods.map((method) => [
              method,
              async (_environment: unknown, context: Context, ...args: unknown[]) => {
                context.check();
                const value = answer(
                  await port.call(
                    service.name.slice(name.length + 2),
                    method,
                    JSON.stringify(args),
                  ),
                );
                context.check();
                return value;
              },
            ]),
          ),
        },
      ]),
    ),
    [isolatedRevision]: true,
    [disposeOrb]: () => port.close(),
  } as AdaptedOrb;
}
