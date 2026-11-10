// Binary host value profile for generated standard WASI calls.
//
// The guest bootstrap and the browser host share this codec. Its layout matches the Rust
// dispatcher generated from the same WIT: little-endian fixed-width numbers, u32-prefixed
// strings and byte lists, u8 option/result tags, u32 enum/variant discriminants and flag bits,
// and safe-integer u64 handles. Values follow jco conventions: 64-bit integers are bigint, list<u8> is a
// Uint8Array and variants are `{ tag, val }`. Inter-Orb portable values are unrelated.
import { TYPES } from "./standard.ts";
import { utf8Decode, utf8Encode } from "./standard-utf8.ts";

export { utf8Decode, utf8Encode };

/** First packet byte, then the operation byte. */
export const STANDARD_PACKET = 0;
export const CALL = 0;
export const STREAM_READ = 1;
export const FUTURE_READ = 2;
export const DROP = 3;
/** Create a guest-writable input stream of a `TYPES` index; replies with its u64 identity. */
export const STREAM_NEW = 4;
/** Write one chunk into a guest-writable input stream; completes once the host accepted it. */
export const STREAM_WRITE = 5;
/** Largest byte chunk either side moves through one stream read or write. */
export const STREAM_CHUNK = 64 << 10;

/** Handle conversion supplied by the side that owns handle identities. */
export interface HandleCodec {
  /** Return the handle number for an outgoing value of `kind`. */
  handle(kind: string, detail: unknown, value: unknown): number;
  /** Return the language value for an incoming handle of `kind`. */
  value(kind: string, detail: unknown, handle: number): unknown;
}

/** Growable little-endian writer. */
export class Writer {
  #bytes = new Uint8Array(64);
  #view = new DataView(this.#bytes.buffer);
  #length = 0;

  /** Reserve space for `count` more bytes. */
  #reserve(count: number): number {
    const start = this.#length;
    if (start + count > this.#bytes.length) {
      const grown = new Uint8Array(Math.max(this.#bytes.length * 2, start + count));
      grown.set(this.#bytes.subarray(0, start));
      this.#bytes = grown;
      this.#view = new DataView(grown.buffer);
    }
    this.#length += count;
    return start;
  }

  // Reserve before reading `#view`/`#bytes`: growth replaces both, and a receiver read first
  // would write into the discarded allocation.
  u8(value: number): void {
    const offset = this.#reserve(1);
    this.#view.setUint8(offset, value);
  }

  u16(value: number): void {
    const offset = this.#reserve(2);
    this.#view.setUint16(offset, value, true);
  }

  u32(value: number): void {
    const offset = this.#reserve(4);
    this.#view.setUint32(offset, value, true);
  }

  u64(value: bigint): void {
    const offset = this.#reserve(8);
    this.#view.setBigUint64(offset, value, true);
  }

  /** Encode a host-issued identity exactly without a process-wide 32-bit ceiling. */
  handle(value: number): void {
    if (!Number.isSafeInteger(value) || value < 0) throw new RangeError("invalid handle identity");
    this.u64(BigInt(value));
  }

  s64(value: bigint): void {
    const offset = this.#reserve(8);
    this.#view.setBigInt64(offset, value, true);
  }

  raw(bytes: Uint8Array): void {
    const offset = this.#reserve(bytes.length);
    this.#bytes.set(bytes, offset);
  }

  bytes(bytes: Uint8Array): void {
    this.u32(bytes.length);
    this.raw(bytes);
  }

  /** Return the written bytes as one exact Uint8Array. */
  finish(): Uint8Array {
    return this.#bytes.slice(0, this.#length);
  }
}

/** Bounds-checked little-endian reader. */
export class Reader {
  #view: DataView;
  #offset = 0;

  constructor(readonly source: Uint8Array) {
    this.#view = new DataView(source.buffer, source.byteOffset, source.byteLength);
  }

  /** Advance over `count` bytes and return their start. */
  #take(count: number): number {
    const start = this.#offset;
    if (start + count > this.source.length) throw new RangeError("standard packet is truncated");
    this.#offset += count;
    return start;
  }

  u8(): number {
    return this.#view.getUint8(this.#take(1));
  }

  u16(): number {
    return this.#view.getUint16(this.#take(2), true);
  }

  u32(): number {
    return this.#view.getUint32(this.#take(4), true);
  }

  u64(): bigint {
    return this.#view.getBigUint64(this.#take(8), true);
  }

  /** Decode a u64 host identity only when JavaScript can represent it exactly. */
  handle(): number {
    const value = this.u64();
    if (value > BigInt(Number.MAX_SAFE_INTEGER)) throw new RangeError("invalid handle identity");
    return Number(value);
  }

  s64(): bigint {
    return this.#view.getBigInt64(this.#take(8), true);
  }

  /** Copy one u32-prefixed byte list. */
  bytes(): Uint8Array {
    return this.raw(this.u32());
  }

  /** Copy exactly `length` bytes. */
  raw(length: number): Uint8Array {
    const start = this.#take(length);
    return this.source.slice(start, start + length);
  }

  string(): string {
    return utf8Decode(this.bytes());
  }

  /** Bytes not yet read. */
  get remaining(): number {
    return this.source.length - this.#offset;
  }

  /** Refuse trailing bytes so one packet has exactly one meaning. */
  end(): void {
    if (this.#offset !== this.source.length)
      throw new RangeError("standard packet has trailing bytes");
  }
}

const NONE = -1;
const U64_MAX = (1n << 64n) - 1n;
const S64_LIMIT = 1n << 63n;

const isSurrogate = (code: number) => code >= 0xd800 && code <= 0xdfff;

/** Refuse a non-integer or out-of-range number instead of wrapping it. */
function integer(value: unknown, minimum: number, maximum: number): number {
  if (typeof value !== "number" || !Number.isInteger(value) || value < minimum || value > maximum)
    throw new RangeError(`integer ${String(value)} is outside ${minimum}..=${maximum}`);
  return value;
}

/** Refuse a 64-bit value that is not exactly representable instead of wrapping it. */
function big(value: unknown, minimum: bigint, maximum: bigint): bigint {
  const exact =
    typeof value === "bigint"
      ? value
      : typeof value === "number" && Number.isSafeInteger(value)
        ? BigInt(value)
        : undefined;
  if (exact === undefined || exact < minimum || exact > maximum)
    throw new RangeError(`64-bit integer ${String(value)} is outside ${minimum}..=${maximum}`);
  return exact;
}

/** Encode one value of generated type `index`. */
export function encode(w: Writer, index: number, value: any, handles: HandleCodec): void {
  const [kind, a, b] = TYPES[index]!;
  switch (kind) {
    case "bool":
      return w.u8(value ? 1 : 0);
    case "u8":
      return w.u8(integer(value, 0, 0xff));
    case "s8":
      return w.u8(integer(value, -0x80, 0x7f) & 0xff);
    case "u16":
      return w.u16(integer(value, 0, 0xffff));
    case "s16":
      return w.u16(integer(value, -0x8000, 0x7fff) & 0xffff);
    case "u32":
      return w.u32(integer(value, 0, 0xffffffff));
    case "s32":
      return w.u32(integer(value, -0x80000000, 0x7fffffff) >>> 0);
    case "u64":
      return w.u64(big(value, 0n, U64_MAX));
    case "s64":
      return w.s64(big(value, -S64_LIMIT, S64_LIMIT - 1n));
    case "char": {
      const text = String(value);
      const code = text.codePointAt(0);
      if (code === undefined || text.length !== (code > 0xffff ? 2 : 1) || isSurrogate(code))
        throw new TypeError("char requires one Unicode scalar value");
      return w.u32(code);
    }
    case "string":
      return w.bytes(utf8Encode(String(value)));
    case "bytes":
      if (!(value instanceof Uint8Array)) throw new TypeError("list<u8> requires a Uint8Array");
      return w.bytes(value);
    case "list":
      w.u32(value.length);
      for (const item of value) encode(w, a as number, item, handles);
      return;
    case "tuple":
      (a as number[]).forEach((type, position) => encode(w, type, value[position], handles));
      return;
    case "record":
      for (const [name, type] of a as [string, number][]) encode(w, type, value[name], handles);
      return;
    case "flags": {
      let bits = 0;
      (a as string[]).forEach((name, bit) => {
        if (value?.[name]) bits |= 1 << bit;
      });
      return w.u32(bits >>> 0);
    }
    case "enum": {
      const position = (a as string[]).indexOf(value);
      if (position < 0) throw new TypeError(`unknown enum case ${value}`);
      return w.u32(position);
    }
    case "variant": {
      const cases = a as [string, number][];
      const position = cases.findIndex(([tag]) => tag === value?.tag);
      if (position < 0) throw new TypeError(`unknown variant case ${value?.tag}`);
      w.u32(position);
      if (cases[position]![1] !== NONE) encode(w, cases[position]![1], value.val, handles);
      return;
    }
    case "option":
      if (value === undefined || value === null) return w.u8(0);
      w.u8(1);
      return encode(w, a as number, value, handles);
    case "result": {
      const ok = value?.tag === "ok";
      if (!ok && value?.tag !== "err") throw new TypeError("result requires { tag, val }");
      w.u8(ok ? 0 : 1);
      const type = (ok ? a : b) as number;
      if (type !== NONE) encode(w, type, value.val, handles);
      return;
    }
    default:
      return w.handle(handles.handle(kind, a, value));
  }
}

/** Decode one value of generated type `index`. */
export function decode(r: Reader, index: number, handles: HandleCodec): any {
  const [kind, a, b] = TYPES[index]!;
  switch (kind) {
    case "bool": {
      const tag = r.u8();
      if (tag > 1) throw new RangeError("invalid bool");
      return tag === 1;
    }
    case "u8":
      return r.u8();
    case "s8":
      return (r.u8() << 24) >> 24;
    case "u16":
      return r.u16();
    case "s16":
      return (r.u16() << 16) >> 16;
    case "u32":
      return r.u32();
    case "s32":
      return r.u32() | 0;
    case "u64":
      return r.u64();
    case "s64":
      return r.s64();
    case "char": {
      const code = r.u32();
      if (code > 0x10ffff || isSurrogate(code)) throw new RangeError("invalid char");
      return String.fromCodePoint(code);
    }
    case "string":
      return r.string();
    case "bytes":
      return r.bytes();
    case "list": {
      const items = [];
      for (let count = r.u32(); count > 0; count--) items.push(decode(r, a as number, handles));
      return items;
    }
    case "tuple":
      return (a as number[]).map((type) => decode(r, type, handles));
    case "record": {
      const record: Record<string, unknown> = {};
      for (const [name, type] of a as [string, number][]) {
        const field = decode(r, type, handles);
        if (field !== undefined) record[name] = field;
      }
      return record;
    }
    case "flags": {
      const bits = r.u32();
      const names = a as string[];
      if (names.length < 32 && bits >>> names.length !== 0)
        throw new RangeError("unknown flag bits");
      return Object.fromEntries(
        (a as string[]).map((name, bit) => [name, (bits & (1 << bit)) !== 0]),
      );
    }
    case "enum": {
      const name = (a as string[])[r.u32()];
      if (name === undefined) throw new RangeError("unknown enum case");
      return name;
    }
    case "variant": {
      const entry = (a as [string, number][])[r.u32()];
      if (!entry) throw new RangeError("unknown variant case");
      const [tag, type] = entry;
      return type === NONE ? { tag } : { tag, val: decode(r, type, handles) };
    }
    case "option": {
      const tag = r.u8();
      if (tag > 1) throw new RangeError("invalid option tag");
      return tag === 0 ? undefined : decode(r, a as number, handles);
    }
    case "result": {
      const tag = r.u8();
      if (tag > 1) throw new RangeError("invalid result tag");
      const ok = tag === 0;
      const type = (ok ? a : b) as number;
      return { tag: ok ? "ok" : "err", val: type === NONE ? undefined : decode(r, type, handles) };
    }
    default:
      return handles.value(kind, a, r.handle());
  }
}
