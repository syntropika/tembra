import { expect, test } from "bun:test";
import { componentLayout } from "../src/interpreter/component/layout.ts";

function integer(value: number): number[] {
  const bytes: number[] = [];
  do {
    const byte = value % 128;
    value = Math.floor(value / 128);
    bytes.push(byte | (value ? 128 : 0));
  } while (value);
  return bytes;
}
function section(id: number, body: readonly number[]): number[] {
  return [id, ...integer(body.length), ...body];
}
function core(data: number, value = 42, pages = 1): number[] {
  return [
    0,
    97,
    115,
    109,
    1,
    0,
    0,
    0,
    ...section(1, [1, 96, 0, 1, 127]),
    ...section(3, [1, 0]),
    ...section(5, [1, 0, pages]),
    ...section(7, [2, 6, 109, 101, 109, 111, 114, 121, 2, 0, 4, 114, 101, 97, 100, 0, 0]),
    ...section(10, [1, 4, 0, 65, value, 11]),
    ...section(11, [1, 0, 65, 0, 11, 1, data]),
  ];
}
function component(data: number, value = 42, pages = 1, metadata = "a"): Uint8Array {
  const name = [...new TextEncoder().encode("tembra:orb")];
  return Uint8Array.of(
    0,
    97,
    115,
    109,
    13,
    0,
    1,
    0,
    ...section(1, core(data, value, pages)),
    ...section(0, [...integer(name.length), ...name, ...new TextEncoder().encode(metadata)]),
  );
}

test("different snapshot bytes share an identity while their actual modules stay separate", async () => {
  const a = componentLayout(component(17))!;
  const b = componentLayout(component(29, 42, 1, "b"))!;
  expect(a.identity).toEqual(b.identity);
  expect(a.modules[0]).not.toEqual(b.modules[0]);
  for (const [layout, value] of [
    [a, 17],
    [b, 29],
  ] as const) {
    const module = await WebAssembly.compile(layout.modules[0]! as BufferSource);
    const instance = await WebAssembly.instantiate(module);
    expect(new Uint8Array((instance.exports.memory as WebAssembly.Memory).buffer)[0]).toBe(value);
  }
});

test("code, memory bounds and non-Tembra custom sections remain in the identity", () => {
  const original = componentLayout(component(17))!.identity;
  expect(componentLayout(component(17, 7))!.identity).not.toEqual(original);
  expect(componentLayout(component(17, 42, 2))!.identity).not.toEqual(original);
  const bytes = component(17);
  const changed = Uint8Array.of(...bytes, ...section(0, [1, 120, 1]));
  expect(componentLayout(changed)!.identity).not.toEqual(original);
});

test("nested components retain unambiguous framing and snapshot module order", () => {
  const inner = component(17);
  const outer = Uint8Array.of(
    0,
    97,
    115,
    109,
    13,
    0,
    1,
    0,
    ...section(4, [...inner]),
    ...section(1, core(29)),
  );
  const layout = componentLayout(outer)!;
  expect(layout.modules).toHaveLength(2);
  expect(layout.modules[0]).toEqual(Uint8Array.from(core(17)));
  expect(layout.modules[1]).toEqual(Uint8Array.from(core(29)));
});

test("caller mutation cannot change captured modules, and unsupported framing declines caching", () => {
  const bytes = component(17);
  const layout = componentLayout(bytes)!;
  const captured = layout.modules[0]!.slice();
  bytes.fill(0);
  expect(layout.modules[0]).toEqual(captured);
  expect(componentLayout(Uint8Array.of(0))).toBeUndefined();
  expect(componentLayout(component(17).slice(0, -1))).toBeUndefined();
  expect(
    componentLayout(Uint8Array.of(0, 97, 115, 109, 13, 0, 1, 0, 1, 255, 255, 255, 255, 127)),
  ).toBeUndefined();
});

test("a generated file template substitutes each Orb's exact module bytes", async () => {
  const { fileTemplate, instantiateFiles } = await import("../src/interpreter/component/layout.ts");
  const a = Uint8Array.from(core(17));
  const b = Uint8Array.from(core(29));
  const shim = Uint8Array.of(0, 97, 115, 109, 1, 0, 0, 0);
  const glue = new TextEncoder().encode("trusted glue");
  const template = await fileTemplate(
    [
      ["orb.js", glue],
      ["orb.core.wasm", a],
      ["orb.core2.wasm", shim],
    ],
    [a],
  );
  expect(template).toBeDefined();
  const files = instantiateFiles(template!, [b]);
  expect(files.get("orb.core.wasm")).toBe(b);
  expect(files.get("orb.core2.wasm")).toBe(shim);
  expect(files.get("orb.js")).toBe(glue);
  expect(a).not.toEqual(b);
});

test("omitted, transformed, duplicated or ambiguous module files decline template caching", async () => {
  const { fileTemplate } = await import("../src/interpreter/component/layout.ts");
  const a = Uint8Array.from(core(17));
  const changed = Uint8Array.from(core(29));
  expect(await fileTemplate([], [a])).toBeUndefined();
  expect(await fileTemplate([["orb.core.wasm", changed]], [a])).toBeUndefined();
  expect(
    await fileTemplate(
      [
        ["a.wasm", a],
        ["b.wasm", a],
      ],
      [a],
    ),
  ).toBeUndefined();
  expect(await fileTemplate([["orb.core.wasm", a]], [a, a])).toBeUndefined();
});

test("component exports and core imports, globals, elements, start and datacount remain in the identity", () => {
  const original = componentLayout(component(17))!.identity;
  for (const [id, payload] of [
    [2, [0]],
    [6, [0]],
    [8, [0]],
    [9, [0]],
    [12, [1]],
  ] as const) {
    // Framing is sufficient here: Rust still performs the actual Wasm validation before this cache.
    const module = [...core(17), ...section(id, payload)];
    const changed = Uint8Array.of(0, 97, 115, 109, 13, 0, 1, 0, ...section(1, module));
    expect(componentLayout(changed)!.identity).not.toEqual(original);
  }
  const bytes = component(17);
  expect(componentLayout(Uint8Array.of(...bytes, ...section(11, [0])))!.identity).not.toEqual(
    original,
  );
});

test("source modules cannot be substituted into an uninstrumented or incorrect generated position", async () => {
  const { fileTemplate, instantiateFiles } = await import("../src/interpreter/component/layout.ts");
  const bytes = Uint8Array.from(core(17));
  expect(await fileTemplate([["helper.wasm", bytes]], [bytes])).toBeUndefined();
  expect(await fileTemplate([["orb.core2.wasm", bytes]], [bytes])).toBeUndefined();
  const template = await fileTemplate([["orb.core.wasm", bytes]], [bytes]);
  expect(() => instantiateFiles(template!, [])).toThrow("module count");
  expect(() => instantiateFiles({ moduleCount: 1, files: [["helper.wasm", 0]] }, [bytes])).toThrow(
    "module position",
  );
});
