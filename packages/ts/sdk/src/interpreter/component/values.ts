/**
 * Typed value conversion between Tembra's host domain form and trusted JCO bindings.
 *
 * The host domain form is the checked wire form with WIT 64-bit positions restored as `bigint`
 * (see `wit-shape.ts`): options are `null` or the value, results and variants are `{ tag, val }`,
 * lists are arrays. JCO bindings use `undefined` for an absent option, typed arrays for numeric
 * lists and, for a function's own `result` return, return the ok value or throw `ComponentError`
 * carrying the err payload. Only the shapes JCO and the contract disagree on are rewritten; the
 * prepared Rust contract stays the single WIT validator of every value that reaches the host.
 */
type WitType = string | number | null | undefined;
interface WitKind {
  type?: WitType;
  list?: WitType;
  option?: WitType;
  record?: { fields: { name: string; type: WitType }[] };
  tuple?: { types: WitType[] };
  variant?: { cases: { name: string; type?: WitType }[] };
  result?: { ok?: WitType; err?: WitType };
  enum?: unknown;
  flags?: unknown;
  handle?: { own?: number; borrow?: number };
  stream?: WitType;
  [other: string]: unknown;
}
export interface TypeGraph {
  readonly javascriptNames: Readonly<Record<string, string>>;
  readonly types: readonly {
    readonly id: number;
    readonly definition: { kind: WitKind | string; owner?: string; name?: string };
  }[];
}
export interface GraphFunction {
  readonly name?: string;
  readonly kind?: string | Readonly<Record<string, number>>;
  readonly params: readonly { readonly name: string; readonly type: WitType }[];
  readonly result?: WitType;
}

/** One direction of conversion; `undefined` means the value is identical in both forms. */
type Convert = ((value: unknown) => unknown) | undefined;
interface Pair {
  /** Host domain form to JCO form. */
  readonly guest: Convert;
  /** JCO form to host domain form. */
  readonly host: Convert;
}

/** Typed array constructors JCO uses for lists of numeric primitives. */
const TYPED: Readonly<Record<string, new (values: ArrayLike<never>) => ArrayLike<unknown>>> = {
  u8: Uint8Array,
  s8: Int8Array,
  u16: Uint16Array,
  s16: Int16Array,
  u32: Uint32Array,
  s32: Int32Array,
  u64: BigUint64Array,
  s64: BigInt64Array,
  f32: Float32Array,
  f64: Float64Array,
} as never;

const IDENTITY: Pair = { guest: undefined, host: undefined };
const apply = (convert: Convert, value: unknown) => (convert ? convert(value) : value);

/** Conversions of one function: arguments in both directions and its result. */
export interface FunctionValues {
  /** Domain arguments to JCO arguments (host calling a guest export). */
  readonly guestArguments: (args: readonly unknown[]) => unknown[];
  /** JCO arguments to domain arguments (guest calling a host import). */
  readonly hostArguments: (args: readonly unknown[]) => unknown[];
  /** Await a JCO export call and answer the domain result (a `result` return is re-tagged). */
  readonly fromGuest: (call: () => unknown) => Promise<unknown>;
  /** Domain result to the value a JCO import implementation returns. */
  readonly toGuest: (value: unknown) => unknown;
  /** Whether the function's own return type is a WIT `result`. */
  readonly tagged: boolean;
}

/** Compile conversions against one contract graph; compiled types are memoized. */
export function converters(graph: TypeGraph): (fn: GraphFunction) => FunctionValues {
  const kinds = new Map(graph.types.map(({ id, definition }) => [id, definition.kind]));
  const memo = new Map<number, Pair>();
  const spelling = (name: string) => {
    const value = graph.javascriptNames[name];
    if (value === undefined) throw new Error(`Missing WIT JavaScript name for ${name}`);
    return value;
  };
  const kindOf = (type: WitType): WitKind | string | undefined =>
    typeof type === "number" ? kinds.get(type) : undefined;
  const resolve = (type: WitType): WitType => {
    let current = type;
    for (let depth = 0; typeof current === "number"; depth++) {
      const kind = kinds.get(current);
      if (typeof kind !== "object" || kind.type === undefined || depth > 64) break;
      current = kind.type;
    }
    return current;
  };
  const compile = (type: WitType): Pair => {
    if (typeof type !== "number") return IDENTITY;
    const known = memo.get(type);
    if (known) return known;
    // Recursive types are impossible in WIT; the placeholder only guards malformed graphs.
    memo.set(type, IDENTITY);
    const pair = build(kindOf(type));
    memo.set(type, pair);
    return pair;
  };
  const build = (kind: WitKind | string | undefined): Pair => {
    if (kind === undefined) throw new Error("Unknown WIT type reference");
    if (typeof kind === "string")
      throw new Error(`WIT ${kind} values are unavailable in browser component execution`);
    if (kind.type !== undefined) return compile(kind.type);
    if (kind.enum !== undefined || kind.flags !== undefined || kind.handle?.own !== undefined)
      return IDENTITY;
    if (kind.list !== undefined) {
      const element = resolve(kind.list);
      const Typed = typeof element === "string" ? TYPED[element] : undefined;
      const item = compile(kind.list);
      return {
        guest: (value) => {
          if (!Array.isArray(value)) throw new TypeError("WIT list requires an array");
          const items = item.guest ? value.map(item.guest) : value;
          return Typed ? new Typed(items as never) : items;
        },
        host: (value) => {
          const items = ArrayBuffer.isView(value)
            ? Array.from(value as unknown as ArrayLike<unknown>)
            : Array.isArray(value)
              ? value
              : (() => {
                  throw new TypeError("WIT list requires an array");
                })();
          return item.host ? items.map(item.host) : [...items];
        },
      };
    }
    if (kind.option !== undefined) {
      const inner = compile(kind.option);
      const innerKind = kindOf(resolve(kind.option));
      // JCO spells a nested option's outer level as a tagged variant.
      const nested = typeof innerKind === "object" && innerKind.option !== undefined;
      return {
        guest: (value) =>
          value === null
            ? nested
              ? { tag: "none" }
              : undefined
            : nested
              ? { tag: "some", val: apply(inner.guest, value) }
              : apply(inner.guest, value),
        host: (value) => {
          if (nested) {
            const tagged = value as { tag?: string; val?: unknown };
            return tagged?.tag === "some" ? apply(inner.host, tagged.val) : null;
          }
          return value === undefined || value === null ? null : apply(inner.host, value);
        },
      };
    }
    if (kind.record) {
      const fields = kind.record.fields.map(
        ({ name, type }) => [spelling(name), compile(type)] as const,
      );
      if (fields.every(([, pair]) => pair === IDENTITY)) return IDENTITY;
      const map = (side: "guest" | "host") => (value: unknown) => {
        if (typeof value !== "object" || value === null)
          throw new TypeError("WIT record requires an object");
        const copy: Record<string, unknown> = { ...(value as object) };
        for (const [name, pair] of fields)
          if (Object.hasOwn(copy, name)) copy[name] = apply(pair[side], copy[name]);
        return copy;
      };
      return { guest: map("guest"), host: map("host") };
    }
    if (kind.tuple) {
      const items = kind.tuple.types.map(compile);
      if (items.every((pair) => pair === IDENTITY)) return IDENTITY;
      const map = (side: "guest" | "host") => (value: unknown) => {
        if (!Array.isArray(value) || value.length !== items.length)
          throw new TypeError("WIT tuple length differs");
        return value.map((item, index) => apply(items[index]![side], item));
      };
      return { guest: map("guest"), host: map("host") };
    }
    if (kind.variant || kind.result) {
      const cases = new Map(
        kind.variant
          ? kind.variant.cases.map(({ name, type }) => [name, type] as const)
          : ([
              ["ok", kind.result!.ok],
              ["err", kind.result!.err],
            ] as const),
      );
      const pairs = new Map(
        [...cases].map(([name, type]) => [name, type == null ? undefined : compile(type)]),
      );
      const map = (side: "guest" | "host") => (value: unknown) => {
        if (typeof value !== "object" || value === null)
          throw new TypeError("WIT tagged value requires an object");
        const { tag, val } = value as { tag?: unknown; val?: unknown };
        if (typeof tag !== "string" || !pairs.has(tag)) throw new TypeError("Unknown WIT case");
        const pair = pairs.get(tag);
        // A payload-less case carries no `val` in the host form, whatever JCO attached.
        return pair === undefined ? { tag } : { tag, val: apply(pair[side], val) };
      };
      return { guest: map("guest"), host: map("host") };
    }
    const name = Object.keys(kind)[0] ?? "unknown";
    throw new Error(`WIT ${name} values are unavailable in browser component execution`);
  };
  return (fn) => {
    const params = fn.params.map(({ type }) => compile(type));
    const result = compile(fn.result);
    const resultKind = kindOf(resolve(fn.result));
    const tagged = typeof resultKind === "object" && resultKind.result !== undefined;
    const tags = tagged ? (resultKind as WitKind).result! : undefined;
    const ok = tags?.ok == null ? undefined : compile(tags.ok);
    const err = tags?.err == null ? undefined : compile(tags.err);
    const each = (side: "guest" | "host") => (args: readonly unknown[]) => {
      if (args.length !== params.length) throw new TypeError("WIT argument count differs");
      return args.map((value, index) => apply(params[index]![side], value));
    };
    return {
      tagged,
      guestArguments: each("guest"),
      hostArguments: each("host"),
      fromGuest: async (call) => {
        if (!tagged) {
          const value = await call();
          return fn.result == null ? null : apply(result.host, value);
        }
        try {
          const value = await call();
          return tags!.ok == null ? { tag: "ok" } : { tag: "ok", val: apply(ok?.host, value) };
        } catch (error) {
          // Only JCO's typed err lane is a business error; anything else is a trap.
          if (error === null || typeof error !== "object" || !Object.hasOwn(error, "payload"))
            throw error;
          const payload = (error as { payload: unknown }).payload;
          return tags!.err == null
            ? { tag: "err" }
            : { tag: "err", val: apply(err?.host, payload) };
        }
      },
      toGuest: (value) => {
        if (fn.result == null) return undefined;
        // JCO accepts an explicit `{ tag, val }` for a `result` return, which is never ambiguous.
        return apply(result.guest, value);
      },
    };
  };
}
