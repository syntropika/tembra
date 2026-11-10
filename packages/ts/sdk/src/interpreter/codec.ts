/**
 * Portable value capture for the isolated-interpreter seam.
 *
 * Hostile JavaScript input is read exactly once and written directly as JSON text. Everything
 * that `JSON.stringify` would coerce silently is refused before any text exists: non-finite
 * numbers, negative zero, functions, symbols, accessors, `toJSON`, class instances, sparse
 * arrays, cycles and DataView buffers. A `bigint` (or a `BigUint64Array`/`BigInt64Array`) is
 * accepted only where the prepared signature's `Shape` marks a WIT `u64`/`s64`, and is written in
 * the same walk as canonical decimal text; no other JavaScript kind is accepted there. This is
 * only a data encoder; the trusted Rust prepared contract remains the single WIT validator and
 * always checks these exact bytes.
 */
import { failure } from "../error.ts";
import type { ContractPort } from "./types.ts";
import { isInteger, type IntegerKind, type Shape } from "./wit-shape.ts";

/** Default ceiling for one captured value, matching the default interpreter packet bound. */
export const VALUE_LIMIT = 8 << 20;
/** Container nesting accepted before the Rust validator's own type-depth limit applies. */
export const VALUE_DEPTH = 64;

const issued = Symbol("tembra.encoded");
const quote = JSON.stringify;
const descriptor = Reflect.getOwnPropertyDescriptor;
const ownKeys = Reflect.ownKeys;
const prototypeOf = Reflect.getPrototypeOf;
const hasOwn = Object.hasOwn;
const isArray = Array.isArray;
const isView = ArrayBuffer.isView;
const objectPrototype = Object.prototype;
const arrayPrototype = Array.prototype;
// Intrinsic typed-array accessors cannot be shadowed by an instance's own properties.
const typedArray = prototypeOf(Uint8Array.prototype) as object;
const typedLength = descriptor(typedArray, "length")!.get as (this: unknown) => number;
const typedTag = descriptor(typedArray, Symbol.toStringTag)!.get as (
  this: unknown,
) => string | undefined;

/**
 * JSON text captured by `encode`. The constructor refuses foreign callers, so raw strings cannot
 * be spliced into interpreter packets without passing through this module.
 */
export class Encoded {
  readonly #text: string;

  /** Only this module can issue captured text. */
  constructor(token: symbol, text: string) {
    if (token !== issued) throw new TypeError("Encoded values are issued by the value codec");
    this.#text = text;
  }

  /** Read captured text; the private-field access rejects forged instances. */
  get text(): string {
    return this.#text;
  }
}

/** Refuse a value before any lossy representation is produced. */
function refused(reason: string): never {
  throw failure(`Portable value rejected: ${reason}`);
}

/** Write one finite number exactly as JSON would, refusing values JSON would change. */
function number(value: number): string {
  if (value !== value || value === Infinity || value === -Infinity)
    refused("non-finite numbers are outside the value profile");
  if (value === 0 && 1 / value < 0) refused("negative zero is outside the value profile");
  return "" + value;
}

/** The 64-bit element kind of a list shape, if any. */
function listKind(shape: Shape | undefined): IntegerKind | undefined {
  return typeof shape === "object" && "list" in shape && typeof shape.list === "string"
    ? shape.list
    : undefined;
}

/**
 * Capture `value` once, bounded by UTF-16 length and container depth. `shape` (from the prepared
 * signature) marks WIT `u64`/`s64` positions; without one, the walk is the untyped capture.
 */
export function encode(value: unknown, limit = VALUE_LIMIT, shape?: Shape): Encoded {
  const ancestors: object[] = [];
  let size = 0;

  /** Account every emitted fragment against the bound before concatenation continues. */
  const emit = (text: string): string => {
    size += text.length;
    if (size > limit) refused("value exceeds its size bound");
    return text;
  };

  const visit = (value: unknown, shape: Shape | undefined): string => {
    if (typeof shape === "string") {
      // A WIT 64-bit position accepts only an in-range bigint primitive.
      if (!isInteger(shape, value))
        refused(
          typeof value === "bigint"
            ? `${shape} value is out of range`
            : `${shape} requires a bigint, not ${value === null ? "null" : typeof value}`,
        );
      return emit(`"${value}"`);
    }
    if (shape !== undefined && "option" in shape) {
      if (value === null || value === undefined) return emit("null");
      shape = shape.option;
      if (typeof shape === "string") return visit(value, shape);
    }
    switch (typeof value) {
      case "string":
        return emit(quote(value));
      case "number":
        return emit(number(value));
      case "boolean":
        return emit(value ? "true" : "false");
      case "undefined":
        // The official bindings spell an absent option as `undefined`; JSON spells it `null`.
        return emit("null");
      case "bigint":
        return refused("BigInt is portable only in a WIT u64/s64 position");
      case "symbol":
      case "function":
        return refused(`${typeof value} values are not portable data`);
    }
    if (value === null) return emit("null");
    const object = value as object;
    if (ancestors.length >= VALUE_DEPTH) refused("value nesting exceeds its limit");
    if (ancestors.includes(object)) refused("cyclic values are not portable");
    ancestors.push(object);
    try {
      if (isView(object)) return typed(object, listKind(shape));
      if (isArray(object)) return list(object as unknown[], shape);
      return record(object, shape);
    } finally {
      ancestors.pop();
    }
  };

  /**
   * Numeric typed arrays follow the official list binding; integer-indexed reads are fixed. A WIT
   * `list<u64>` takes a `BigUint64Array` and `list<s64>` a `BigInt64Array`, each element written
   * as canonical decimal text; 64-bit arrays are refused anywhere else.
   */
  const typed = (view: ArrayBufferView, kind: IntegerKind | undefined): string => {
    const tag = typedTag.call(view);
    if (tag === undefined) refused("DataView is not a portable list");
    const wide = tag === "BigInt64Array" || tag === "BigUint64Array";
    if (kind !== undefined && tag !== (kind === "u64" ? "BigUint64Array" : "BigInt64Array"))
      refused(`list<${kind}> requires a ${kind === "u64" ? "BigUint64Array" : "BigInt64Array"}`);
    if (kind === undefined && wide) refused("64-bit lists are portable only as WIT list<u64/s64>");
    const elements = view as unknown as ArrayLike<number | bigint>;
    const length = typedLength.call(view);
    let text = emit("[");
    for (let index = 0; index < length; index++) {
      if (index) text += emit(",");
      const element = elements[index]!;
      text += emit(typeof element === "bigint" ? `"${element}"` : number(element));
    }
    return text + emit("]");
  };

  /** Read each element once; a hole is refused instead of becoming `null`. */
  const list = (array: unknown[], shape: Shape | undefined): string => {
    const length = array.length;
    if (!Number.isSafeInteger(length) || length < 0 || length > limit - size)
      refused("list length exceeds its size bound");
    const items = typeof shape === "object" && "items" in shape ? shape.items : undefined;
    if (items && items.length !== length) refused("tuple length differs from WIT");
    const item = typeof shape === "object" && "list" in shape ? shape.list : undefined;
    let text = emit("[");
    for (let index = 0; index < length; index++) {
      if (index) text += emit(",");
      const element = descriptor(array, "" + index);
      if (!element) refused("sparse arrays are not portable");
      if (!element.enumerable || !hasOwn(element, "value"))
        refused("array accessors and non-enumerable elements are not portable");
      text += visit(element.value, items ? items[index] : item);
    }
    if (prototypeOf(array) !== arrayPrototype) refused("array subclasses are not portable data");
    if (ownKeys(array).length !== length + 1)
      refused("extra array properties are not portable data");
    return text + emit("]");
  };

  /**
   * Read each own data property once; accessors, symbols and hidden fields are refused. With a
   * record shape, a key WIT does not declare is refused. With a variant or result shape, the at
   * most two descriptors are read first, so `tag` selects the type of `val` in any key order.
   */
  const record = (object: object, shape: Shape | undefined): string => {
    const prototype = prototypeOf(object);
    if (prototype !== objectPrototype && prototype !== null)
      refused("only plain objects, arrays and numeric typed arrays are portable");
    const fields = typeof shape === "object" && "fields" in shape ? shape.fields : undefined;
    const cases = typeof shape === "object" && "cases" in shape ? shape.cases : undefined;
    const property = (key: string | symbol): unknown => {
      if (typeof key !== "string") refused("symbol-keyed properties are not portable");
      const found = descriptor(object, key);
      if (found === undefined || !found.enumerable || !hasOwn(found, "value"))
        refused("accessors and non-enumerable properties are not portable");
      return found.value;
    };
    let text = emit("{");
    if (cases) {
      const keys = ownKeys(object);
      if (keys.length > 2) refused("WIT tagged values have only tag and val");
      const entries = keys.map((key) => [key as string, property(key)] as const);
      const tag = entries.find(([key]) => key === "tag")?.[1];
      if (typeof tag !== "string" || !cases.has(tag)) refused("WIT tagged value needs a known tag");
      entries.forEach(([key, value], index) => {
        const type = key === "val" ? cases.get(tag) : undefined;
        text += emit(index ? "," : "") + emit(quote(key)) + emit(":") + visit(value, type);
      });
      return text + emit("}");
    }
    let first = true;
    for (const key of ownKeys(object)) {
      const value = property(key);
      if (fields && !fields.has(key as string))
        refused(`record field ${quote(key)} is not declared by WIT`);
      text +=
        emit(first ? "" : ",") +
        emit(quote(key)) +
        emit(":") +
        visit(value, fields?.get(key as string));
      first = false;
    }
    return text + emit("}");
  };

  let text: string;
  try {
    text = visit(value, shape);
  } catch (error) {
    // Revoked proxies and throwing traps surface as ordinary value rejections.
    throw failure(error);
  }
  return new Encoded(issued, text);
}

/** The prepared validator's exact success envelope; any other answer is decoded as an error. */
const ACCEPTED = '{"ok":null}';

/**
 * Check captured bytes through a slot bound at installation. The same `Encoded` instance is then
 * the only value that may be spliced into a packet, so no second serialization can diverge.
 */
export function check(
  contract: ContractPort,
  slot: number,
  arguments_: boolean,
  value: Encoded,
): Encoded {
  const answer = contract.validate(slot, arguments_, value.text);
  if (answer === ACCEPTED) return value;
  let error: unknown;
  try {
    error = (JSON.parse(answer) as { error?: unknown }).error;
  } catch {
    error = undefined;
  }
  throw failure(error ?? "Interpreter value validation failed");
}

/** Check the guest's original bytes before decoding them into the outer JavaScript heap. */
export function checkedResult(contract: ContractPort, slot: number, text: string): Encoded {
  return check(contract, slot, false, new Encoded(issued, text));
}

/** Host replies preserve an already checked capture rather than decoding and serializing it. */
export function reply(value: unknown): string {
  return value instanceof Encoded ? value.text : encode(value ?? null).text;
}

/**
 * Build a guest packet from fixed control fields and one captured value. Control values are
 * primitives encoded by the platform JSON writer; the value is spliced only from `Encoded`.
 */
export function packet(
  control: Readonly<Record<string, string | number | null>>,
  field: string,
  value: Encoded,
): string {
  const head = quote(control);
  return `${head.slice(0, -1)}${head.length > 2 ? "," : ""}${quote(field)}:${value.text}}`;
}
