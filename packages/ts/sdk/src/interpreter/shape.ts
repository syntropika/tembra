import { Lifetime } from "../bridge/identifiers.ts";
import { camel } from "./types.ts";

export interface Description {
  name?: string;
  apiVersion?: number;
  services: Record<
    string,
    { methods: string[]; lifetime?: Lifetime; linger?: number; access?: boolean }
  >;
}

/** Reject incomplete or extra executable callbacks before a backend publishes kernel metadata. */
export function checkShape(
  description: Description,
  name: string,
  apiVersion: number,
  services: readonly { name: string; methods: string[] }[],
): void {
  if (
    (description.name !== undefined && description.name !== name) ||
    (description.apiVersion !== undefined && description.apiVersion !== apiVersion)
  )
    throw new Error("Bundle identity differs from its registration");
  const expected = new Set<string>();
  for (const service of services) {
    const binding = service.name.slice(name.length + 2);
    const alias = camel(binding.split("/").at(-1)!.split("@")[0]!);
    expected.add(alias);
    const implementation = description.services[alias];
    if (
      !implementation ||
      [...implementation.methods].sort().join("\0") !== service.methods.map(camel).sort().join("\0")
    )
      throw new Error(`Implementation methods differ from WIT for ${binding}`);
    if (implementation.access)
      throw new Error("Service access policies require a prepared view adapter");
    if (
      implementation.lifetime !== undefined &&
      !Object.values(Lifetime).includes(implementation.lifetime)
    )
      throw new Error("Invalid service lifetime");
    if (
      implementation.linger !== undefined &&
      (!Number.isInteger(implementation.linger) ||
        implementation.linger < 0 ||
        implementation.linger > 0xffffffff)
    )
      throw new Error("Invalid service linger");
  }
  if (Object.keys(description.services).some((alias) => !expected.has(alias)))
    throw new Error("Bundle exports services absent from its WIT world");
}
