import type { NodeComponentPort } from "@syntropika/tembra-engine-node";
import type { OrbDefinition } from "../authoring/types.ts";
import type { Context } from "../runtime/context.ts";
import { signature, disposeOrb, isolatedRevision, type AdaptedOrb } from "#tembra-adapters";
import { answer } from "./types.ts";
import { encode } from "./codec.ts";

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
      plan.orb.services.map((service) => {
        const prefix = `${name}::`;
        if (!service.name.startsWith(prefix)) throw new Error("Invalid prepared component service");
        const binding = service.name.slice(prefix.length);
        return [
          service.name,
          {
            [signature]: service.signature,
            methods: Object.fromEntries(
              service.methods.map((method) => [
                method,
                async (_environment: unknown, context: Context, ...args: unknown[]) => {
                  context.check();
                  const value = answer(await port.call(binding, method, encode(args).text));
                  context.check();
                  return value;
                },
              ]),
            ),
          },
        ];
      }),
    ),
    [isolatedRevision]: true,
    [disposeOrb]: () => port.close(),
  } as AdaptedOrb;
}
