import type { Static, TSchema } from "typebox";

import { ContractError } from "./error.js";
import { canonicalJson, clonePortable, fingerprint } from "./portable.js";
import type { JsonValue } from "./portable.js";
import {
  assertUnambiguousInput,
  canonicalIdentity,
  normalizeInput,
  normalizeSchema,
  prepareSchema,
  object,
} from "./schema.js";
import type { Schema } from "./schema.js";

/** Remote provider routing: a fixed Host name, or prefix plus a named normalized input field. */
export type Placement =
  | { readonly name: string }
  | { readonly prefix: string; readonly key: string };
/** Portable schema or a named native capability; native identities are meaningful only locally. */
export type ValueDescriptor = Schema | { readonly native: string };
/** Canonical member metadata; omitted method payload fields mean absence rather than null. */
export type MemberDescriptor =
  | {
      readonly kind: "method";
      readonly args?: ValueDescriptor;
      readonly result?: ValueDescriptor;
    }
  | { readonly kind: "state"; readonly schema: Schema }
  | { readonly kind: "opaque"; readonly id: string };
/** Canonical portable contract metadata, verified independently of any implementation or Host. */
export type Descriptor = {
  readonly descriptorVersion: 1;
  readonly name: string;
  readonly version: number;
  readonly kind: "local" | "remote";
  readonly input: Schema;
  readonly placement?: Placement;
  readonly variants: Readonly<Record<string, Readonly<Record<string, MemberDescriptor>>>>;
  /** Lowercase SHA-256 of the canonical descriptor body, excluding this field. */
  readonly fingerprint: string;
};

declare const nativeType: unique symbol;
/** Local-only nominal channel metadata. A matching TypeScript type does not confer native authority. */
export type NativeSchema<T = unknown> = {
  readonly native: string;
  readonly [nativeType]?: (value: T) => T;
};
/** Local-only provider capability member; the backend checks the issued resource at binding time. */
export type Opaque<T = unknown> = {
  readonly kind: "opaque";
  readonly id: string;
  readonly [nativeType]?: (value: T) => T;
};
/** Authoring signature for a callable member; construction grants no invocation authority. */
export type Method<A = undefined, R = undefined> = {
  readonly kind: "method";
  readonly args?: A;
  readonly result?: R;
};
/** A method whose payload positions use portable schemas, local native identities or absence. */
export type MethodDefinition = Method<
  TSchema | NativeSchema | undefined,
  TSchema | NativeSchema | undefined
>;
/** Local declaration member: callable method, core-managed state schema or opaque capability. */
export type LocalMember = MethodDefinition | Opaque | TSchema;
/** Named local members compiled into the contract's single local view. */
export type LocalMembers = Readonly<Record<string, LocalMember>>;
/** One remote view; callers see only its declared portable state and methods. */
export type VariantDefinition = {
  readonly state?: Readonly<Record<string, TSchema>>;
  readonly methods?: Readonly<Record<string, MethodDefinition>>;
};
/** Named remote views selected by the provider; a reference exposes its currently selected view. */
export type Variants = Readonly<Record<string, VariantDefinition>>;

/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** Frozen contract declaration and input-identity helpers; it holds no runtime object. */
export type ContractToken<I = TSchema, M = LocalMembers, V = Variants> = {
  readonly descriptor: Descriptor;
  readonly name: string;
  readonly kind: "local" | "remote";
  readonly input: I;
  readonly localMembers?: M;
  readonly remoteVariants?: V;
  /** Applies supported defaults and input cleaning, then validates. Omitted/null input means {}. */
  normalizeInput(input?: unknown): JsonValue;
  /** Canonical normalized input JSON with values equal to declared defaults omitted. */
  identity(input?: unknown): string;
  /** Normalizes input and resolves remote placement; local contracts return undefined. */
  resolveHost(input?: unknown): string | undefined;
};
/* oxlint-enable typescript/method-signature-style */
/** Local-only members, including native capabilities; use with a local executor binding. */
export type LocalContract<I extends TSchema = TSchema, M extends LocalMembers = LocalMembers> = {
  readonly kind: "local";
  readonly localMembers: M;
} & ContractToken<I, M>;
/** Portable variants and placement rules used to acquire a reference on its selected Host. */
export type RemoteContract<I extends TSchema = TSchema, V extends Variants = Variants> = {
  readonly kind: "remote";
  readonly remoteVariants: V;
} & ContractToken<I, LocalMembers, V>;
/** Common token surface for APIs accepting local, remote or generated declarations. */
export type AnyContract = ContractToken;
declare const generatedTypes: unique symbol;
/** Type-only metadata emitted by a verified descriptor's external generator. */
export type GeneratedContract<I, R> = {
  readonly [generatedTypes]: { readonly input: I; readonly reference: R };
} & ContractToken;
/** Caller input inferred from the declared schema or a generated contract's metadata. */
export type InputOf<C> =
  C extends GeneratedContract<infer I, unknown>
    ? I
    : C extends ContractToken<infer I>
      ? I extends TSchema
        ? Static<I>
        : never
      : never;
/** Value carried by a portable or native schema; an omitted schema means undefined. */
export type Infer<S> =
  S extends NativeSchema<infer T> ? T : S extends TSchema ? Static<S> : undefined;

/**
 * Declares a method signature. Omission means no payload, whereas Type.Null() means present null.
 * Use method(undefined, Type.Integer()) for a no-argument method returning an integer.
 * This declaration neither registers nor executes a handler.
 */
export function method(args?: undefined, result?: undefined): Method;
export function method<const R extends TSchema | NativeSchema>(
  args: undefined,
  result: R,
): Method<undefined, R>;
export function method<const A extends TSchema | NativeSchema>(
  args: A,
  result?: undefined,
): Method<A>;
export function method<
  const A extends TSchema | NativeSchema,
  const R extends TSchema | NativeSchema,
>(args: A, result: R): Method<A, R>;
export function method(
  args?: TSchema | NativeSchema,
  result?: TSchema | NativeSchema,
): Method<TSchema | NativeSchema | undefined, TSchema | NativeSchema | undefined> {
  return {
    kind: "method",
    ...(args === undefined ? {} : { args }),
    ...(result === undefined ? {} : { result }),
  };
}
function nominal<T>(id: T): asserts id is T & string {
  if (typeof id !== "string" || !/^[^\p{White_Space}@]+@[0-9]+$/u.test(id)) {
    throw new ContractError("native_id", "nominal ID must be name@version");
  }
}
/** Declares a local nominal channel using a non-whitespace name@version identifier. */
export function native<T>(id: string): NativeSchema<T> {
  nominal(id);
  return Object.freeze({ native: id });
}
/** Declares a local opaque member using a non-whitespace name@version identifier. */
export function opaque<T>(id: string): Opaque<T> {
  nominal(id);
  return Object.freeze({ kind: "opaque", id });
}
/** Routes a remote contract to one nonempty provider Host name. */
export function hostNamed(name: string): Placement {
  if (typeof name !== "string" || !name) {
    throw new ContractError("placement", "host name must be nonempty text");
  }
  return Object.freeze({ name });
}
/** Routes to prefix + input[key]; the normalized input field must be a nonempty string. */
export function hostFamily(prefix: string, key: string): Placement {
  if (typeof prefix !== "string" || typeof key !== "string" || !prefix || !key) {
    throw new ContractError("placement", "prefix and key must be nonempty text");
  }
  return Object.freeze({ prefix, key });
}

const reserved = new Set([
  "then",
  "status",
  "ready",
  "release",
  "variant",
  "shape",
  "contract",
  "error",
  "onShape",
  "onStatus",
]);
function exactKeys(
  value: Record<string, unknown>,
  allowed: readonly string[],
  reason: string,
): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) {
      throw new ContractError(reason, `unknown field ${key}`);
    }
  }
}
function valueDescriptor(value: unknown, kind: "local" | "remote"): ValueDescriptor {
  if (
    value !== null &&
    typeof value === "object" &&
    !Array.isArray(value) &&
    Object.hasOwn(value, "native")
  ) {
    const marker = object(value);
    exactKeys(marker, ["native"], "native_id");
    nominal(marker.native);
    if (kind === "remote") {
      throw new ContractError("member", "native signatures are local only");
    }
    return { native: marker.native };
  }
  return prepareSchema(value);
}

function freeze<T>(value: T): T {
  if (value !== null && typeof value === "object") {
    for (const child of Object.values(value)) {
      freeze(child);
    }
    Object.freeze(value);
  }
  return value;
}

type DescriptorMap<T> = Record<string, T>;
function emptyRecord<T>() {
  const record: DescriptorMap<T> = {};
  Object.setPrototypeOf(record, null);
  return record;
}
function descriptorPlacement(source: unknown, input: Schema): Placement {
  const value = object(source, "placement");
  if (Object.hasOwn(value, "name")) {
    exactKeys(value, ["name"], "placement");
    if (typeof value.name !== "string") {
      throw new ContractError("placement", "host name must be text");
    }
    return hostNamed(value.name);
  }
  exactKeys(value, ["prefix", "key"], "placement");
  if (typeof value.prefix !== "string" || typeof value.key !== "string") {
    throw new ContractError("placement", "family values must be text");
  }
  const placement = hostFamily(value.prefix, value.key);
  if (
    typeof input === "boolean" ||
    !Array.isArray(input.required) ||
    !input.required.includes(value.key) ||
    object(object(input.properties ?? {})[value.key]).type !== "string"
  ) {
    throw new ContractError("placement", "family key must be a required string property");
  }
  return placement;
}
function descriptorMember(value: unknown, kind: "local" | "remote"): MemberDescriptor {
  const member = object(value, "member");
  if (member.kind === "method") {
    exactKeys(member, ["kind", "args", "result"], "member");
    return {
      kind: "method",
      ...(Object.hasOwn(member, "args") ? { args: valueDescriptor(member.args, kind) } : {}),
      ...(Object.hasOwn(member, "result") ? { result: valueDescriptor(member.result, kind) } : {}),
    };
  } else if (member.kind === "state") {
    exactKeys(member, ["kind", "schema"], "member");
    return {
      kind: "state",
      schema: prepareSchema(member.schema),
    };
  } else if (member.kind === "opaque") {
    exactKeys(member, ["kind", "id"], "member");
    nominal(member.id);
    if (kind === "remote") {
      throw new ContractError("member", "opaque members are local only");
    }
    return { kind: "opaque", id: member.id };
  }
  throw new ContractError("member", "unknown member kind");
}

type DescriptorSource = Record<string, unknown> &
  Pick<Descriptor, "descriptorVersion" | "name" | "version" | "kind">;
function assertDescriptorMetadata(raw: Record<string, unknown>): asserts raw is DescriptorSource {
  if (
    raw.descriptorVersion !== 1 ||
    typeof raw.name !== "string" ||
    !raw.name ||
    typeof raw.version !== "number" ||
    !Number.isSafeInteger(raw.version) ||
    raw.version <= 0 ||
    (raw.kind !== "local" && raw.kind !== "remote")
  ) {
    throw new ContractError("descriptor", "invalid contract metadata");
  }
}
function descriptor(source: unknown, external: boolean): Descriptor {
  const raw = object(clonePortable(source), "descriptor");
  exactKeys(
    raw,
    [
      "descriptorVersion",
      "name",
      "version",
      "kind",
      "input",
      "placement",
      "variants",
      "fingerprint",
    ],
    "descriptor",
  );
  assertDescriptorMetadata(raw);
  const input = prepareSchema(raw.input);
  assertUnambiguousInput(input);
  const sourceVariants = object(raw.variants, "descriptor");
  const variants = emptyRecord<Record<string, MemberDescriptor>>();
  const names = Object.keys(sourceVariants);
  if (
    raw.kind === "local"
      ? raw.placement !== undefined || names.length !== 1 || names[0] !== ""
      : raw.placement === undefined || !names.length || names.includes("")
  ) {
    throw new ContractError("descriptor", "scope/variants/placement do not agree");
  }
  const placement = raw.kind === "remote" ? descriptorPlacement(raw.placement, input) : undefined;
  for (const variant of names) {
    const members = object(sourceVariants[variant], "member");
    const output = emptyRecord<MemberDescriptor>();
    for (const [name, value] of Object.entries(members)) {
      if (!name || reserved.has(name)) {
        throw new ContractError("member", `reserved or empty member ${name}`);
      }
      output[name] = descriptorMember(value, raw.kind);
    }
    variants[variant] = output;
  }
  const normalized: Omit<Descriptor, "fingerprint"> = {
    descriptorVersion: 1,
    name: raw.name,
    version: raw.version,
    kind: raw.kind,
    input,
    ...(placement ? { placement } : {}),
    variants,
  };
  const computed = fingerprint(normalized);
  if (
    external &&
    (typeof raw.fingerprint !== "string" || !/^[a-f0-9]{64}$/u.test(raw.fingerprint))
  ) {
    throw new ContractError(
      "fingerprint",
      "external descriptor requires lowercase SHA-256 fingerprint",
    );
  }
  if (raw.fingerprint !== undefined && raw.fingerprint !== computed) {
    throw new ContractError("fingerprint", "descriptor digest mismatch");
  }
  return freeze({ ...normalized, fingerprint: computed });
}

function token(description: Descriptor): ContractToken {
  return {
    descriptor: description,
    name: description.name,
    kind: description.kind,
    input: description.input,
    normalizeInput: (input = null) => normalizeInput(description.input, input),
    identity: (input = null) => canonicalIdentity(description.input, input),
    resolveHost(input = null) {
      const value = normalizeInput(description.input, input);
      const { placement } = description;
      if (!placement) {
        return undefined;
      }
      if ("name" in placement) {
        return placement.name;
      }
      const normalized = object(value, "placement");
      const suffix = normalized[placement.key];
      if (typeof suffix !== "string" || !suffix) {
        throw new ContractError("placement", "family input must name a nonempty suffix");
      }
      return placement.prefix + suffix;
    },
  };
}

/**
 * Verifies untrusted descriptor structure, supported schemas and fingerprint, then copies and
 * deeply freezes a token. Throws ContractError on malformed metadata before returning a token.
 */
export function fromDescriptor(source: unknown): ContractToken {
  return freeze(token(descriptor(source, true)));
}
/** Canonical descriptor body used for hashing; excludes the fingerprint field. */
export function descriptorBytes(contract: ContractToken): string {
  const { fingerprint: _digest, ...data } = contract.descriptor;
  return canonicalJson(data);
}

/**
 * Declares a same-Host contract with portable state/methods and optional native capabilities.
 * Validates the descriptor and deeply freezes the retained authoring schemas/member records.
 * Invalid declarations throw ContractError; no runtime object is created.
 *
 * @example
 * ```ts
 * import { local, method, Type } from "@tembra/sdk";
 * const Counter = local("counter", {
 *   version: 1,
 *   input: Type.Object({}),
 *   members: { count: Type.Integer(), increment: method(Type.Integer(), Type.Integer()) },
 * });
 * Counter.identity({}); // "{}"
 * ```
 */
export function local<const I extends TSchema, const M extends LocalMembers>(
  name: string,
  definition: {
    readonly version: number;
    readonly input: I;
    readonly members: M;
  },
): LocalContract<I, M> {
  const members = Object.fromEntries(
    Object.entries(definition.members).map(([key, value]) => {
      const member = object(value);
      return [
        key,
        member.kind === "method" || member.kind === "opaque"
          ? authorMember(member)
          : { kind: "state", schema: normalizeSchema(value) },
      ];
    }),
  );
  const data = descriptor(
    {
      descriptorVersion: 1,
      name,
      version: definition.version,
      kind: "local",
      input: normalizeSchema(definition.input),
      variants: { "": members },
    },
    false,
  );
  return freeze({
    ...token(data),
    kind: "local" as const,
    input: definition.input,
    localMembers: definition.members,
  });
}

type AuthoredMethod = { kind: "method"; args?: ValueDescriptor; result?: ValueDescriptor };
function authorMember(member: Record<string, unknown>): MemberDescriptor {
  if (member.kind === "opaque") {
    nominal(member.id);
    return { kind: "opaque", id: member.id };
  }
  const output: AuthoredMethod = {
    kind: "method",
  };
  for (const key of ["args", "result"] as const) {
    if (Object.hasOwn(member, key)) {
      output[key] = valueDescriptor(member[key], "local");
    }
  }
  return output;
}

/**
 * Declares a provider-routed contract with named portable views. Native/opaque channels are
 * refused. A variant describes visible state and methods; it does not itself authorize access.
 * Validates and deeply freezes the retained input schema and variant records; throws ContractError
 * for invalid placement, schemas or member declarations without acquiring a provider.
 */
export function remote<const I extends TSchema, const V extends Variants>(
  name: string,
  definition: {
    readonly version: number;
    readonly input: I;
    readonly host: Placement;
    readonly variants: V;
  },
): RemoteContract<I, V> {
  const variants = Object.fromEntries(
    Object.entries(definition.variants).map(([variantName, value]) => {
      const states = Object.entries(value.state ?? {}).map(([key, schema]) => [
        key,
        { kind: "state", schema: normalizeSchema(schema) },
      ]);
      const methods = Object.entries(value.methods ?? {}).map(([key, methodDefinition]) => {
        if (Object.hasOwn(value.state ?? {}, key)) {
          throw new ContractError("duplicate", "member is both state and method");
        }
        return [key, authorMember(object(methodDefinition))];
      });
      return [variantName, Object.fromEntries([...states, ...methods])];
    }),
  );
  const data = descriptor(
    {
      descriptorVersion: 1,
      name,
      version: definition.version,
      kind: "remote",
      input: normalizeSchema(definition.input),
      placement: definition.host,
      variants,
    },
    false,
  );
  return freeze({
    ...token(data),
    kind: "remote" as const,
    input: definition.input,
    remoteVariants: definition.variants,
  });
}
