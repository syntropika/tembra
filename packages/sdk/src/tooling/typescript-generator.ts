import { ContractError, canonicalJson, fromDescriptor } from "@tembra/sdk";
import type { MemberDescriptor, Schema, ValueDescriptor } from "@tembra/sdk";
import { compile } from "json-schema-to-typescript";
import type { JSONSchema } from "json-schema-to-typescript";

/** Names and local capability types for a generated consumer; generation performs no file writes. */
type GeneratorOptions = {
  /** Public contract export name; defaults to a valid identifier derived from the contract name. */
  readonly name?: string;
  /** TypeScript type expressions keyed by nominal capability ID; every used local capability needs one. */
  readonly nativeTypes?: Readonly<Record<string, string>>;
};

function byName<T>([left]: readonly [string, T], [right]: readonly [string, T]): number {
  return left < right ? -1 : Number(left > right);
}

function identifier(value: string): string {
  const name = value
    .replaceAll(/(?<digit>\d)(?<letter>[a-zA-Z])/gu, "$<digit> $<letter>")
    .split(/[^a-zA-Z0-9]+/u)
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
    .join("");
  return /^[A-Za-z_$]/u.test(name) ? name : `Contract${name}`;
}

/**
 * Generates DTOs, contract metadata and reference declarations from a verified descriptor snapshot.
 * Captures name and native mappings before yielding; later caller mutation does not affect generation.
 * Refusal writes no files. Missing native mappings reject with ContractError/native_mapping.
 * Invalid descriptors and reserved/invalid export names also reject before a consumer is returned.
 * Returned source imports @tembra/sdk and binds references through the existing BindingPort;
 * it adds no runtime object owner. Persist it in a generated file, separate from authored code.
 *
 * Internal backend for `generateBindings`; callers configure TypeScript outputs through the
 * public tooling API rather than importing this module.
 */
export async function generateTypeScript(
  source: unknown,
  options: GeneratorOptions = {},
): Promise<string> {
  const contract = fromDescriptor(source);
  const { descriptor } = contract;
  const name = options.name ?? identifier(descriptor.name);
  const nativeTypes = { ...options.nativeTypes };
  const dtoPrefix = identifier(name);
  const reserved = new Set([
    "BindingPort",
    "ContextHandle",
    "ContractReference",
    "GeneratedContract",
    "Readable",
    "bindReference",
    "fromDescriptor",
    "parsePortableJson",
    "class",
    "function",
    "default",
    "const",
    "let",
    "var",
    "interface",
    "export",
    "import",
    "enum",
    "new",
    "return",
    "await",
    "yield",
    "extends",
    "implements",
    "private",
    "protected",
    "public",
    "static",
    "super",
    "this",
    "null",
    "true",
    "false",
  ]);
  if (!/^[A-Za-z_$][\w$]*$/u.test(name) || reserved.has(name)) {
    throw new ContractError("codegen", "export name must be a nonreserved TypeScript identifier");
  }
  const chunks: string[] = [
    "// Generated from a verified Tembra descriptor. Do not edit.",
    'import { bindReference, fromDescriptor, parsePortableJson, type BindingPort, type ContextHandle, type ContractReference, type GeneratedContract, type Readable } from "@tembra/sdk";',
  ];
  const used = new Set<string>([name, `${name}Reference`, `bind${name}`]);
  const typeName = (suffix: string) => {
    const base = dtoPrefix + identifier(suffix);
    let result = base;
    let serial = 2;
    while (used.has(result)) {
      result = `${base}${serial}`;
      serial += 1;
    }
    used.add(result);
    return result;
  };
  async function dto(schema: Schema, preferred: string): Promise<string> {
    const generatedName = typeName(preferred);
    const normalized: JSONSchema =
      typeof schema === "boolean"
        ? { tsType: schema ? "unknown" : "never" }
        : { ...schema, title: generatedName };
    chunks.push(
      await compile(normalized, generatedName, {
        bannerComment: "",
        additionalProperties: true,
        unknownAny: true,
        strictIndexSignatures: true,
        declareExternallyReferenced: true,
        unreachableDefinitions: false,
        customName(candidate, key) {
          if (candidate.title === generatedName) {
            return generatedName;
          }
          const localName = candidate.title ?? key;
          return localName === undefined ? undefined : generatedName + identifier(localName);
        },
      }),
    );
    return generatedName;
  }
  function nativeType(id: string): string {
    const mapped = nativeTypes[id];
    if (mapped === undefined || mapped.trim().length === 0) {
      throw new ContractError(
        "native_mapping",
        `local capability ${id} requires an explicit TypeScript type mapping`,
      );
    }
    return mapped;
  }
  async function valueType(schema: ValueDescriptor, preferred: string): Promise<string> {
    return typeof schema !== "boolean" && "native" in schema && typeof schema.native === "string"
      ? nativeType(schema.native)
      : await dto(schema, preferred);
  }
  async function memberField(
    member: MemberDescriptor,
    property: string,
    prefix: string,
  ): Promise<string> {
    if (member.kind === "method") {
      const args =
        member.args === undefined
          ? ""
          : `args: ${await valueType(member.args, `${prefix}_Args`)}, `;
      const result =
        member.result === undefined
          ? "undefined"
          : await valueType(member.result, `${prefix}_Result`);
      return `  readonly ${property}: (${args}context?: ContextHandle) => Promise<${result}>;`;
    }
    if (member.kind === "state") {
      return `  readonly ${property}: Readable<${await dto(member.schema, `${prefix}_State`)}>;`;
    }
    return `  readonly ${property}: ${nativeType(member.id)};`;
  }
  const inputName = await dto(descriptor.input, "Input");
  const variantTypes: string[] = [];
  for (const [variant, members] of Object.entries(descriptor.variants).toSorted(byName)) {
    const fields: string[] = [];
    for (const [memberName, member] of Object.entries(members).toSorted(byName)) {
      const property = JSON.stringify(memberName);
      const prefix = `${variant || "Local"}_${memberName}`;
      // oxlint-disable-next-line no-await-in-loop -- DTO compilation allocates names and appends shared chunks in canonical member order.
      fields.push(await memberField(member, property, prefix));
    }
    const variantName = typeName(`${variant || "Local"}_Reference`);
    chunks.push(
      `export interface ${variantName} extends ContractReference {\n  readonly variant: ${variant ? `${JSON.stringify(variant)} | undefined` : "undefined"};\n${fields.join("\n")}\n}`,
    );
    variantTypes.push(variantName);
  }
  chunks.push(
    `export type ${name}Reference = ${variantTypes.join(" | ")};`,
    `export const ${name} = fromDescriptor(parsePortableJson(${JSON.stringify(canonicalJson(descriptor))})) as GeneratedContract<${inputName}, ${name}Reference>;`,
    `export function bind${name}(port: BindingPort): ${name}Reference {\n  return bindReference(${name}, port);\n}`,
    `export type ${name}AcquisitionInput = ${inputName};`,
  );
  return `${chunks.join("\n\n")}\n`;
}
