/**
 * WIT `u64`/`s64` on the portable executor wire: JavaScript sees `bigint`, the wire carries a
 * canonical decimal JSON string, and only a position that WIT types as `u64`/`s64` is ever read
 * that way (a WIT `string` stays a string). This module holds the parts shared by the hardened
 * host capture, the host lift and the guest conversion:
 * - the strict integer helpers;
 * - `Shape`: where 64-bit values occur in one WIT type, compiled once per prepared function;
 * - `lift`: restoring `bigint` in data parsed from already checked text.
 * It has no imports and no state, so the guest bootstrap bundles it as is. It is not a WIT
 * validator: the Rust contract still checks every value in full.
 */

export type IntegerKind = "u64" | "s64";

const U64_MAX = (1n << 64n) - 1n;
const S64_MIN = -(1n << 63n);
const S64_MAX = (1n << 63n) - 1n;
/** Canonical decimal text: no sign on `u64`, no `+`, no `-0`, no leading zeros or whitespace. */
const CANONICAL = { u64: /^(?:0|[1-9][0-9]{0,19})$/, s64: /^(?:0|-?[1-9][0-9]{0,18})$/ };

/** Whether `value` is a bigint primitive within the range of `kind`. */
export function isInteger(kind: IntegerKind, value: unknown): value is bigint {
  return (
    typeof value === "bigint" &&
    (kind === "u64" ? value >= 0n && value <= U64_MAX : value >= S64_MIN && value <= S64_MAX)
  );
}

/** Canonical wire text of a `bigint`; every other JavaScript kind is refused. */
export function integerText(kind: IntegerKind, value: unknown): string {
  if (!isInteger(kind, value))
    throw new TypeError(
      typeof value === "bigint"
        ? `${kind} value is out of range`
        : `${kind} requires a bigint, not ${typeof value}`,
    );
  // String concatenation applies the abstract ToString to a bigint primitive.
  return "" + value;
}

/** The `bigint` of canonical wire text; anything else is refused. */
export function integerValue(kind: IntegerKind, text: unknown): bigint {
  if (typeof text !== "string" || !CANONICAL[kind].test(text))
    throw new TypeError(`${kind} requires canonical decimal text`);
  const value = BigInt(text);
  if (!isInteger(kind, value)) throw new TypeError(`${kind} value is out of range`);
  return value;
}

/** Where 64-bit values occur in one WIT type; `undefined` means none below this point. */
export type Shape =
  | IntegerKind
  | { readonly list: Shape }
  /** Record fields by JavaScript name; every field is present, 64-bit-free ones as `undefined`. */
  | { readonly fields: ReadonlyMap<string, Shape | undefined> }
  /** Tuples and argument lists. */
  | { readonly items: readonly (Shape | undefined)[] }
  | { readonly option: Shape }
  /** Variant and result payloads by case name; every case is present. */
  | { readonly cases: ReadonlyMap<string, Shape | undefined> };

/** Argument list and result shapes of one prepared function. */
export interface Signature {
  readonly params: Shape | undefined;
  readonly result: Shape | undefined;
}

type WitType = string | number | null | undefined;
interface WitKind {
  type?: WitType;
  list?: WitType;
  option?: WitType;
  record?: { fields: { name: string; type: WitType }[] };
  tuple?: { types: WitType[] };
  variant?: { cases: { name: string; type?: WitType }[] };
  result?: { ok?: WitType; err?: WitType };
}
/** The parts of the contract's `describe` answer this module reads. */
export interface WitTypes {
  readonly javascriptNames: Readonly<Record<string, string>>;
  readonly types: readonly { readonly id: number; readonly definition: { kind: WitKind } }[];
}
export interface WitFunction {
  readonly params: readonly { readonly name: string; readonly type: WitType }[];
  readonly result?: WitType;
}

/** Compile function signatures against one contract graph; compiled types are memoized. */
export function shapes(graph: WitTypes): (fn: WitFunction) => Signature {
  const kinds = new Map(graph.types.map(({ id, definition }) => [id, definition.kind]));
  const compiled = new Map<number, Shape | undefined>();
  const field = (name: string) => {
    const spelling = graph.javascriptNames[name];
    if (spelling === undefined) throw new Error(`Missing WIT JavaScript name for ${name}`);
    return spelling;
  };
  const cases = (entries: [string, WitType][]): Shape | undefined => {
    const shapes = new Map(entries.map(([name, type]) => [name, compile(type)] as const));
    return [...shapes.values()].some(Boolean) ? { cases: shapes } : undefined;
  };
  const compile = (type: WitType): Shape | undefined => {
    if (type === "u64" || type === "s64") return type;
    if (typeof type !== "number") return undefined;
    if (compiled.has(type)) return compiled.get(type);
    const kind = kinds.get(type);
    if (!kind) throw new Error("Unknown WIT type reference");
    let shape: Shape | undefined;
    if (kind.type !== undefined) shape = compile(kind.type);
    else if (kind.list !== undefined) {
      const item = compile(kind.list);
      shape = item && { list: item };
    } else if (kind.option !== undefined) {
      const item = compile(kind.option);
      shape = item && { option: item };
    } else if (kind.record) {
      const fields = new Map(
        kind.record.fields.map(({ name, type }) => [field(name), compile(type)] as const),
      );
      shape = [...fields.values()].some(Boolean) ? { fields } : undefined;
    } else if (kind.tuple) {
      const items = kind.tuple.types.map(compile);
      shape = items.some(Boolean) ? { items } : undefined;
    } else if (kind.variant)
      shape = cases(kind.variant.cases.map(({ name, type }) => [name, type] as [string, WitType]));
    else if (kind.result)
      shape = cases([
        ["ok", kind.result.ok],
        ["err", kind.result.err],
      ]);
    compiled.set(type, shape);
    return shape;
  };
  return (fn) => {
    const items = fn.params.map(({ type }) => compile(type));
    return {
      params: items.some(Boolean) ? { items } : undefined,
      result: compile(fn.result),
    };
  };
}

/**
 * Restore `bigint` positions in data parsed from text the Rust contract already checked. Shaped
 * containers are copied, never updated in place, so the caller's parsed data stays wire form;
 * lists of 64-bit values become arrays of `bigint`.
 */
export function lift(shape: Shape | undefined, value: unknown): unknown {
  if (shape === undefined || value === null || value === undefined) return value;
  if (typeof shape === "string") return integerValue(shape, value);
  if ("option" in shape) return lift(shape.option, value);
  if ("list" in shape) {
    if (!Array.isArray(value)) throw new TypeError("WIT list requires an array");
    return value.map((item) => lift(shape.list, item));
  }
  if ("items" in shape) {
    if (!Array.isArray(value) || value.length !== shape.items.length)
      throw new TypeError("WIT tuple length differs");
    return value.map((item, index) => lift(shape.items[index], item));
  }
  if (typeof value !== "object" || Array.isArray(value))
    throw new TypeError("WIT record or tagged value requires an object");
  const copy: Record<string, unknown> = { ...value };
  if ("fields" in shape) {
    for (const [name, item] of shape.fields)
      if (item && Object.hasOwn(copy, name)) copy[name] = lift(item, copy[name]);
  } else {
    const item = shape.cases.get(copy.tag as string);
    if (item && Object.hasOwn(copy, "val")) copy.val = lift(item, copy.val);
  }
  return copy;
}
