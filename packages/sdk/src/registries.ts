import { ContractError } from "./error.js";
import type { Registry } from "./executor.js";
import { canonicalJson } from "./portable.js";
import type { JsonValue } from "./portable.js";

type PortableMetadata<T> = T extends JsonValue
  ? T
  : T extends (...args: never[]) => unknown
    ? never
    : T extends readonly unknown[]
      ? { [K in keyof T]: PortableMetadata<T[K]> }
      : T extends object
        ? { [K in keyof T]: K extends string | number ? PortableMetadata<T[K]> : never }
        : never;

/**
 * Describes portable contribution metadata without attaching a registry to a Host.
 * The generic is a static contract; the Rust bridge still checks every submitted
 * value for JSON portability, and Rust owns policy, versions and contribution lifetime.
 */
export function defineRegistry<T = JsonValue>(
  name: [T] extends [PortableMetadata<T>] ? string : never,
  options: { readonly conflict?: "error" | "last" } = {},
): Registry<T> {
  const conflict = options.conflict ?? "error";
  if (typeof name !== "string" || name.length === 0) {
    throw new ContractError("registry_name", "registry name must be nonempty text");
  }
  if (conflict !== "error" && conflict !== "last") {
    throw new ContractError("registry", "invalid registry conflict policy");
  }
  const definition = { name, conflict };
  canonicalJson(definition);
  return Object.freeze(definition);
}
