import { expect, test } from "bun:test";
import { TembraError } from "../src/error.ts";
import { Encoded, check, encode, packet } from "../src/interpreter/codec.ts";
import { decodeHostRequest } from "../src/interpreter/host-protocol.ts";
import { Scope } from "../src/interpreter/scope.ts";
import type { ContractPort } from "../src/interpreter/types.ts";

/** Expect a typed value rejection before any text is produced. */
function refused(value: unknown, reason: RegExp): void {
  let error: unknown;
  try {
    encode(value);
  } catch (caught) {
    error = caught;
  }
  expect(error).toBeInstanceOf(TembraError);
  expect((error as Error).message).toMatch(reason);
}

test("benign portable values encode exactly as JSON", () => {
  for (const value of [
    null,
    true,
    0,
    -1.5,
    2 ** 60,
    1e21,
    5e-324,
    'quote" \\ \n   😀',
    [],
    {},
    [1, [2, [3]], { a: "b" }],
    { b: 1, a: [null, false], 2: "integer keys first", __proto__: null },
    Object.assign(Object.create(null) as object, { nested: { list: [0.1, 0.2] } }),
  ])
    expect(encode(value).text).toBe(JSON.stringify(value));
});

test("lossy numbers and non-data values are refused before encoding", () => {
  refused(-0, /negative zero/);
  refused([1, -0], /negative zero/);
  refused({ value: NaN }, /non-finite/);
  refused([Infinity], /non-finite/);
  refused(-Infinity, /non-finite/);
  refused(1n, /BigInt/);
  refused([2n ** 64n], /BigInt/);
  refused(Symbol("s"), /symbol/);
  refused({ callback() {} }, /function/);
  refused(new Date(0), /plain objects/);
  refused(new Map(), /plain objects/);
  refused(new (class Point {})(), /plain objects/);
  refused(Object(1), /plain objects/);
  refused(new DataView(new ArrayBuffer(2)), /DataView/);
  refused(new BigInt64Array(1), /64-bit/);
  refused(new BigUint64Array(1), /64-bit/);
  refused(new Float32Array([-0]), /negative zero/);
  refused(new Float64Array([NaN]), /non-finite/);
});

test("toJSON, accessors and symbol or hidden properties are never invoked or dropped", () => {
  let called = 0;
  refused(
    {
      toJSON() {
        called++;
        return 1;
      },
    },
    /function/,
  );
  refused(
    {
      get value() {
        called++;
        return 1;
      },
    },
    /accessors/,
  );
  refused({ [Symbol("hidden")]: 1 }, /symbol-keyed/);
  refused(Object.defineProperty({}, "hidden", { value: 1, enumerable: false }), /non-enumerable/);
  expect(called).toBe(0);
  // A non-callable `toJSON` field is ordinary data and stays visible to the contract.
  expect(encode({ toJSON: "field" }).text).toBe('{"toJSON":"field"}');
});

test("undefined spells an absent option while holes, cycles, depth and size are refused", () => {
  expect(encode(undefined).text).toBe("null");
  expect(encode([1, undefined]).text).toBe("[1,null]");
  // The official bindings spell `none` as undefined; JSON.stringify would drop the field.
  expect(encode({ count: undefined, name: "a" }).text).toBe('{"count":null,"name":"a"}');
  // A missing field stays missing so the contract can still refuse an incomplete record.
  expect(encode({ name: "a" }).text).toBe('{"name":"a"}');
  // eslint-disable-next-line no-sparse-arrays
  refused([1, , 3], /sparse/);
  refused(new Array(2), /sparse/);
  const cycle: Record<string, unknown> = {};
  cycle.self = { cycle };
  refused(cycle, /cyclic/);
  const listCycle: unknown[] = [];
  listCycle.push(listCycle);
  refused(listCycle, /cyclic/);
  // Shared acyclic branches are ordinary repeated data.
  const shared = { x: 1 };
  expect(encode([shared, shared]).text).toBe('[{"x":1},{"x":1}]');
  let deep: unknown = 1;
  for (let index = 0; index < 64; index++) deep = [deep];
  expect(encode(deep).text).toBe(JSON.stringify(deep));
  refused([deep], /nesting/);
  expect(encode("x".repeat(100), 102).text).toHaveLength(102);
  expect(() => encode("x".repeat(100), 101)).toThrow(/size bound/);
  expect(() => encode(new Array(1000).fill(0), 100)).toThrow(/size bound/);
});

test("typed arrays follow the official list binding through intrinsic reads", () => {
  expect(encode(new Uint8Array([0, 255])).text).toBe("[0,255]");
  expect(encode(new Int16Array([-2, 3])).text).toBe("[-2,3]");
  expect(encode(new Float32Array([0.5, 1.25])).text).toBe("[0.5,1.25]");
  expect(encode(new Uint8ClampedArray(new Uint8Array([7]).buffer)).text).toBe("[7]");
  expect(encode({ bytes: new Uint8Array(new ArrayBuffer(8), 2, 3) }).text).toBe(
    '{"bytes":[0,0,0]}',
  );
  // An own `length` accessor on a typed array cannot change what is read.
  let called = 0;
  const shadowed = Object.defineProperty(new Uint8Array([1, 2]), "length", {
    get() {
      called++;
      return 100;
    },
  });
  expect(encode(shadowed).text).toBe("[1,2]");
  expect(called).toBe(0);
});

test("proxies are observed once and encoded from that single observation", () => {
  const reads = new Map<PropertyKey, number>();
  let generation = 0;
  const target = { a: 1, b: 2 };
  const changing = new Proxy(target, {
    getOwnPropertyDescriptor(object, key) {
      reads.set(key, (reads.get(key) ?? 0) + 1);
      // Every observation returns a fresh value; the text holds the one actually seen.
      return { value: ++generation, writable: true, enumerable: true, configurable: true };
    },
    get() {
      throw new Error("records are read through descriptors only");
    },
  });
  expect(encode(changing).text).toBe('{"a":1,"b":2}');
  expect([...reads.values()]).toEqual([1, 1]);

  let elementReads = 0;
  const list = new Proxy([10, 20], {
    getOwnPropertyDescriptor(array, key) {
      elementReads++;
      return Reflect.getOwnPropertyDescriptor(array, key);
    },
    get(array, key, receiver) {
      if (key !== "length") throw new Error("list elements are captured through descriptors");
      return Reflect.get(array, key, receiver);
    },
  });
  expect(encode(list).text).toBe("[10,20]");
  expect(elementReads).toBe(2);

  // A proxy lying about its prototype is still read only as own data.
  const disguised = new Proxy(new Date(0), {
    getPrototypeOf: () => Object.prototype,
    ownKeys: () => ["when"],
    getOwnPropertyDescriptor: () => ({
      value: "now",
      writable: true,
      enumerable: true,
      configurable: true,
    }),
  });
  expect(encode(disguised).text).toBe('{"when":"now"}');

  const { proxy, revoke } = Proxy.revocable({}, {});
  revoke();
  refused(proxy, /revoked/);
  refused(
    new Proxy(
      {},
      {
        ownKeys() {
          throw new Error("hostile trap");
        },
      },
    ),
    /hostile trap/,
  );
});

test("captured text cannot be forged or spliced without the codec", () => {
  expect(() => new Encoded(Symbol("tembra.encoded"), "{}")).toThrow(TypeError);
  const forged = Object.create(Encoded.prototype) as Encoded;
  expect(() => forged.text).toThrow(TypeError);
  const text = packet(
    { kind: "invoke", environment: 4, service: 'a"b', method: "m" },
    "args",
    encode([{ x: [1] }]),
  );
  expect(JSON.parse(text)).toEqual({
    kind: "invoke",
    environment: 4,
    service: 'a"b',
    method: "m",
    args: [{ x: [1] }],
  });
  expect(JSON.parse(packet({}, "value", encode(null)))).toEqual({ value: null });
});

test("the contract checks exactly the captured bytes and refusals stay typed", () => {
  const seen: string[] = [];
  const contract: ContractPort = {
    bind: () => 0,
    request: () => '{"ok":null}',
    validate(slot, arguments_, value) {
      seen.push(`${slot}:${arguments_}:${value}`);
      return value === "[1]"
        ? '{"ok":null}'
        : '{"error":{"code":"invalid","reason":"wit_value","message":"value does not match U32"}}';
    },
  };
  const captured = encode([1]);
  expect(check(contract, 3, true, captured)).toBe(captured);
  expect(() => check(contract, 3, false, encode(["1"]))).toThrow(/does not match U32/);
  expect(seen).toEqual(["3:true:[1]", '3:false:["1"]']);
  const malformed: ContractPort = { ...contract, validate: () => "not json" };
  expect(() => check(malformed, 0, true, captured)).toThrow(TembraError);
});

test("snapshot reads accept only a decimal known revision", () => {
  expect(decodeHostRequest('{"operation":"state.read","id":2,"known":"17"}')).toEqual({
    operation: "state.read",
    id: 2,
    known: "17",
  });
  expect(decodeHostRequest('{"operation":"reference.state","id":2,"known":null}')).toEqual({
    operation: "reference.state",
    id: 2,
  });
  for (const known of ['""', "17", '"-1"', '"1.5"', '"x"', `"${"9".repeat(21)}"`])
    expect(() => decodeHostRequest(`{"operation":"state.read","id":2,"known":${known}}`)).toThrow(
      /Malformed/,
    );
  expect(() => decodeHostRequest('{"operation":"state.update","id":2,"known":"1"}')).toThrow(
    /Malformed/,
  );
});

test("unchanged state reads omit the value but still check ownership and lifetime", () => {
  let revision = "4";
  let closed = false;
  const declaration = {
    environment: { id: 7, object: { id: 70 } },
    orb: undefined,
    session: { snapshot: () => ({ revision, version: 2 }) },
    state: (initial: unknown) => ({
      get value() {
        if (closed) throw new Error("State owner is gone");
        return initial;
      },
    }),
  };
  const scope = new Scope({} as never, declaration as never, null);
  const declaring = scope.exchange({ declaration: declaration as never });
  const id = declaring.sync({
    operation: "state.declare",
    initial: { a: [1] },
    version: 2,
    validate: null,
  }) as number;
  const reader = scope.exchange({ validation: true });
  expect(reader.sync({ operation: "state.read", id })).toEqual({
    value: { a: [1] },
    revision: "4",
    version: 2,
  });
  expect(reader.sync({ operation: "state.read", id, known: "4" })).toEqual({
    revision: "4",
    version: 2,
  });
  revision = "5";
  expect(reader.sync({ operation: "state.read", id, known: "4" })).toEqual({
    value: { a: [1] },
    revision: "5",
    version: 2,
  });
  expect(() => reader.sync({ operation: "state.read", id: id + 1, known: "5" })).toThrow(
    /Unknown state handle/,
  );
  expect(() => reader.sync({ operation: "state.read", scope: 99, id, known: "5" })).toThrow(
    /Foreign environment handle/,
  );
  closed = true;
  expect(() => reader.sync({ operation: "state.read", id, known: "5" })).toThrow(/gone/);
});

test("array capture refuses indexed accessors without invoking them and refuses inherited holes", () => {
  let called = 0;
  const accessor = Object.defineProperty([0], "0", {
    enumerable: true,
    get() {
      called++;
      return 7;
    },
  });
  refused(accessor, /accessor/);
  expect(called).toBe(0);
  refused(Object.setPrototypeOf(new Array(1), { 0: 9 }), /sparse/);
  refused(Object.defineProperty([1], "0", { value: 1, enumerable: false }), /non-enumerable/);
});

test("array capture refuses subclasses and extra own properties without observing their values", () => {
  class Numbers extends Array<number> {}
  refused(new Numbers(1, 2), /subclasses/);
  refused(Object.assign([1, 2], { extra: 3 }), /extra array properties/);
  refused(Object.assign([1], { [Symbol("extra")]: 2 }), /extra array properties/);
  let observed = 0;
  refused(
    Object.defineProperty([1], "extra", {
      get() {
        observed++;
        return 2;
      },
    }),
    /extra array properties/,
  );
  expect(observed).toBe(0);
});
