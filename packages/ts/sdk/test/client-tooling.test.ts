import { expect, test } from "bun:test";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { client } from "../src/tooling/index.ts";
import { createHost } from "../src/node.ts";

const sdk = fileURLToPath(new URL("../", import.meta.url));

/** Every WIT kind of the portable value profile, with kebab-case and reserved names. */
const CONTRACT = `package example:values@1.0.0;
interface shapes {
  enum mode { fast, slow }
  flags access { read, write-all }
  record item { name: string, count: option<u32>, big-number: u64 }
  variant shape { none, size(u32), named(item) }
  type items = list<item>;
  numbers: func(small: s32, wide: f64, byte: u8, signed: s64) -> u64;
  collect: func(values: items) -> list<u8>;
  tagged: func(value: shape, mode: mode, access: access) -> result<item, string>;
  deep: func(value: list<list<u32>>, pair: tuple<u64, string>) -> char;
  nothing: func();
  maybe: func(value: option<string>) -> option<item>;
  plain: func() -> result;
  kebab-name: async func(first-value: u32, delete: bool, context: list<shape>) -> bool;
  check: func(public: bool, %static: u32, let: string, arguments: list<u8>, eval: f32, implements: s8, %await: bool) -> bool;
}
world provider { export shapes; }
`;

/** A call site: the valid calls compile and each wrong argument is a reported error. */
const SITE = `import type { Host } from "@syntropika/tembra-sdk/runtime";
import type { Context } from "@syntropika/tembra-sdk";
import {
  get,
  IDENTITY,
  type Access,
  type Item,
  type Mode,
  type Shape,
  type Shapes,
} from "./example-values-shapes.ts";

export async function check(host: Host, context: Context) {
  const identity: "example:values/shapes@1.0.0" = IDENTITY;
  const fromHost: Shapes = await get(host, { orb: "values", key: "main" });
  const fromContext = await get(context, { orb: "values", key: ["team", "a"] }, { principal: "p" });
  const total: bigint = await fromHost.numbers(1, 2.5, 3, 4n);
  const withContext: bigint = await fromHost.numbers(1, 2.5, 3, 4n, context);
  const bytes: number[] = await fromHost.collect([{ name: "a", count: null, bigNumber: 1n }]);
  const item: Item = { name: "a", count: 2, bigNumber: 3n };
  const shape: Shape = { tag: "named", val: item };
  const mode: Mode = "fast";
  const access: Access = { writeAll: true };
  const tagged = await fromHost.tagged(shape, mode, access);
  const count: number | null = tagged.tag === "ok" ? tagged.val.count : tagged.val.length;
  const char: string = await fromHost.deep([[1], []], [5n, "x"]);
  const nothing: void = await fromHost.nothing();
  const maybe: Item | null = await fromHost.maybe(null);
  const structural: { name: string; count: number | null; bigNumber: bigint } | null = maybe;
  const plain: { tag: "ok" } | { tag: "err" } = await fromHost.plain();
  const flag: boolean = await fromContext.kebabName(1, true, [{ tag: "none" }]);
  const checked: boolean = await fromHost.check(true, 1, "x", [1], 1.5, -1, false);
  fromHost.reference.release();
  // @ts-expect-error A u64 argument is a bigint, not a number.
  fromHost.numbers(1, 2.5, 3, 4);
  // @ts-expect-error A numeric string is not a u64.
  fromHost.numbers(1, 2.5, 3, "4");
  // @ts-expect-error An absent option is null, not undefined.
  fromHost.maybe(undefined);
  // @ts-expect-error Record fields are camelCase.
  fromHost.collect([{ name: "a", count: null, "big-number": 1n }]);
  // @ts-expect-error An enum value is one of its cases.
  fromHost.tagged({ tag: "none" }, "medium", {});
  // @ts-expect-error A variant case carries its own payload.
  fromHost.tagged({ tag: "size", val: "big" }, "fast", {});
  // @ts-expect-error Methods keep the WIT argument count.
  fromHost.nothing(1);
  // @ts-expect-error Kebab-case keys are not methods of the client.
  fromHost["kebab-name"](1, true, []);
  // @ts-expect-error A named record keeps its WIT fields.
  const partial: Item = { name: "a", count: null };
  // @ts-expect-error A named enum is one of its cases.
  const slow: Mode = "medium";
  // @ts-expect-error A result stays tagged, never unwrapped.
  const unwrapped: { name: string } = await fromHost.tagged({ tag: "none" }, "fast", {});
  return { identity, total, withContext, bytes, count, char, nothing, maybe, structural, plain, flag, checked, unwrapped, partial, slow };
}
`;

/** Typecheck files under the SDK's project options, with the SDK resolved from source. */
function diagnostics(files: readonly string[]): Map<string, string[]> {
  const config = ts.readConfigFile(path.join(sdk, "tsconfig.json"), ts.sys.readFile);
  const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, sdk);
  const program = ts.createProgram(files, {
    ...parsed.options,
    noEmit: true,
    baseUrl: sdk,
    paths: {
      "@syntropika/tembra-sdk": [path.join(sdk, "src/index.ts")],
      "@syntropika/tembra-sdk/runtime": [path.join(sdk, "src/host.ts")],
    },
  });
  const reported = new Map(files.map((file) => [file, [] as string[]]));
  for (const diagnostic of ts.getPreEmitDiagnostics(program)) {
    const file = diagnostic.file?.fileName;
    if (file !== undefined && reported.has(file))
      reported.get(file)!.push(ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n"));
  }
  return reported;
}

test("a generated client names and types every WIT kind in the host's value form and checks call sites", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
  );
  try {
    await writeFile(path.join(root, "contract.wit"), CONTRACT);
    const out = path.join(root, "clients");
    const files = await client({ wit: path.join(root, "contract.wit"), world: "provider", out });
    expect(files).toEqual([path.join(out, "example-values-shapes.ts")]);
    const generated = await readFile(files[0]!, "utf8");
    expect(generated).toContain('export const IDENTITY = "example:values/shapes@1.0.0";');
    expect(generated).toContain(
      "numbers(small: number, wide: number, byte: number, signed: bigint, context?: Context): Promise<bigint>;",
    );
    // Each record, variant, enum and flags is exported under its WIT name; an alias is not.
    expect(generated).toContain(
      "/** The WIT record `item`. */\nexport type Item = { name: string; count: number | null; bigNumber: bigint };",
    );
    expect(generated).toContain(
      '/** The WIT variant `shape`. */\nexport type Shape = { tag: "none" } | { tag: "size"; val: number } | { tag: "named"; val: Item };',
    );
    expect(generated).toContain('/** The WIT enum `mode`. */\nexport type Mode = "fast" | "slow";');
    expect(generated).toContain(
      "/** The WIT flags `access`. */\nexport type Access = { read?: boolean; writeAll?: boolean };",
    );
    expect(generated).not.toContain("Items");
    expect(generated).toContain("collect(values: Item[], context?: Context): Promise<number[]>;");
    expect(generated).toContain(
      'tagged(value: Shape, mode: Mode, access: Access, context?: Context): Promise<{ tag: "ok"; val: Item } | { tag: "err"; val: string }>;',
    );
    expect(generated).toContain(
      "maybe(value: string | null, context?: Context): Promise<Item | null>;",
    );
    expect(generated).toContain(
      "deep(value: number[][], pair: [bigint, string], context?: Context): Promise<string>;",
    );
    expect(generated).toContain("nothing(context?: Context): Promise<void>;");
    expect(generated).toContain(
      'plain(context?: Context): Promise<{ tag: "ok" } | { tag: "err" }>;',
    );
    expect(generated).toContain(
      "kebabName(firstValue: number, delete_: boolean, context_: Shape[], context?: Context): Promise<boolean>;",
    );
    expect(generated).toContain(
      'kebabName: (...args) => reference["kebab-name"](...sent(args, 3)),',
    );
    expect(generated).toContain(
      "check(public_: boolean, static_: number, let_: string, arguments_: number[], eval_: number, implements_: number, await_: boolean, context?: Context): Promise<boolean>;",
    );
    // The generated module and the call site typecheck where the signatures agree with the WIT.
    const site = path.join(out, "site.ts");
    const wrong = path.join(out, "wrong.ts");
    await writeFile(site, SITE);
    await writeFile(
      wrong,
      'import { get } from "./example-values-shapes.ts";\nexport const wrong = (scope: Parameters<typeof get>[0]) => get(scope, { orb: "values", key: "main" }).then((shapes) => shapes.numbers(1, 2.5, 3, 4));\n',
    );
    const reported = diagnostics([files[0]!, site, wrong]);
    expect(reported.get(files[0]!)).toEqual([]);
    expect(reported.get(site)).toEqual([]);
    expect(reported.get(wrong)!.join("\n")).toContain(
      "Argument of type 'number' is not assignable to parameter of type 'bigint'",
    );
    // Regeneration leaves an unchanged module untouched.
    const before = await stat(files[0]!, { bigint: true });
    await client({ wit: path.join(root, "contract.wit"), world: "provider", out });
    expect((await stat(files[0]!, { bigint: true })).mtimeNs).toBe(before.mtimeNs);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("a generated client acquires the kebab-case service and forwards each call unchanged", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
  );
  try {
    await writeFile(path.join(root, "contract.wit"), CONTRACT);
    const [file] = await client({
      wit: path.join(root, "contract.wit"),
      world: "provider",
      out: root,
    });
    const { get, IDENTITY } = (await import(file!)) as {
      IDENTITY: string;
      get: (scope: unknown, address: unknown, options?: unknown) => Promise<any>;
    };
    expect(IDENTITY).toBe("example:values/shapes@1.0.0");
    const acquisitions: unknown[] = [];
    const reference = {
      "kebab-name": (...args: unknown[]) => Promise.resolve(args),
      numbers: (...args: unknown[]) => Promise.resolve(args.length),
      nothing: (...args: unknown[]) => Promise.resolve(args.length),
      release: () => {},
    };
    const scope = {
      get: (service: string, key: unknown, options: unknown) => {
        acquisitions.push([service, key, options]);
        return Promise.resolve(reference);
      },
    };
    const shapes = await get(scope, { orb: "values", key: ["team", "a"] }, { principal: "p" });
    expect(acquisitions).toEqual([
      ["values::example:values/shapes@1.0.0", ["team", "a"], { principal: "p" }],
    ]);
    expect(shapes.reference).toBe(reference);
    expect(await shapes.kebabName(7, true, [{ tag: "none" }])).toEqual([
      7,
      true,
      [{ tag: "none" }],
    ]);
    const context = { marker: "a trailing context travels with the arguments" };
    expect(await shapes.numbers(1, 2, 3, 4n, context)).toBe(5);
    expect(await shapes.numbers(1, 2, 3, 4n)).toBe(4);
    // A caller that forwards an optional context passes `undefined`, which is not an argument.
    const forward = (context?: unknown) => shapes.numbers(1, 2, 3, 4n, context);
    expect(await forward()).toBe(4);
    expect(await shapes.nothing(undefined)).toBe(0);
    expect(shapes["kebab-name"]).toBeUndefined();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("client generation refuses resources, a world without callable exports and a non-WIT entry", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
  );
  try {
    await writeFile(
      path.join(root, "contract.wit"),
      "package example:refused; interface sessions { resource session { add: func(value: u32) -> u32; } open: func() -> session; } interface types-only { record value { count: u32 } } world handles { export sessions; } world silent { export types-only; }",
    );
    await expect(
      client({ wit: path.join(root, "contract.wit"), world: "handles", out: root }),
    ).rejects.toThrow("example:refused/sessions declares WIT resources");
    await expect(
      client({ wit: path.join(root, "contract.wit"), world: "silent", out: root }),
    ).rejects.toThrow("exports no interface a host can call");
    await expect(client({ wit: path.join(root, "contract.txt"), out: root })).rejects.toThrow(
      "requires a .wit entry file",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("client generation refuses functions a reference's own members would hide", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
  );
  try {
    for (const [declaration, name] of [
      ["release: func(id: u32)", "release"],
      ["then: func()", "then"],
      ["status: func() -> u32", "status"],
      ["state: func() -> string", "state"],
      ["ready: func() -> bool", "ready"],
      ["opaque: func(name: string)", "opaque"],
      ["reference: func()", "reference"],
    ]) {
      await writeFile(
        path.join(root, "contract.wit"),
        `package example:hidden; interface members { ${declaration}; } world app { export members; }`,
      );
      await expect(
        client({ wit: path.join(root, "contract.wit"), world: "app", out: root }),
      ).rejects.toThrow(`${name} collides with a member every client has`);
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

/** WIT names TypeScript reads otherwise: `new`, `Object`'s members, the module's own types. */
const NAMES = `package example:names@1.0.0;
interface calls {
  record calls { value: u32 }
  record context { value: u32 }
  wrap: func(value: calls, scope: context) -> calls;
  new: func(value: u32) -> u32;
  %constructor: func() -> string;
  to-string: func() -> string;
  to-locale-string: func() -> string;
  value-of: func() -> u32;
  has-own-property: func(name: string) -> bool;
  is-prototype-of: func() -> bool;
  property-is-enumerable: func() -> bool;
}
interface context { add: func(value: u32) -> u32; }
interface promise { add: func(value: u32) -> u32; }
interface acquisition-options { add: func(value: u32) -> u32; }
interface dynamic-reference { add: func(value: u32) -> u32; }
interface reference-base { add: func(value: u32) -> u32; }
world app {
  export calls;
  export context;
  export promise;
  export acquisition-options;
  export dynamic-reference;
  export reference-base;
}
`;

/** A call site of those clients, next to the SDK's own `Context`. */
const NAMES_SITE = `import type { Context } from "@syntropika/tembra-sdk";
import { get, type Calls, type Calls_, type Context_ } from "./example-names-calls.ts";
import * as acquisition from "./example-names-acquisition-options.ts";
import * as context from "./example-names-context.ts";
import * as dynamic from "./example-names-dynamic-reference.ts";
import * as promise from "./example-names-promise.ts";
import * as base from "./example-names-reference-base.ts";

export async function check(scope: Parameters<typeof get>[0], ctx?: Context) {
  const address = { orb: "names", key: "main" };
  const calls: Calls = await get(scope, address);
  const made: number = await calls.new(1, ctx);
  const text: string = await calls.toString();
  const owned: boolean = await calls.hasOwnProperty("name", ctx);
  const scoped: Context_ = { value: 2 };
  const wrapped: Calls_ = await calls.wrap({ value: 1 }, scoped, ctx);
  // @ts-expect-error \`new\` is a method that takes its WIT parameters.
  calls.new("1");
  // @ts-expect-error \`toString\` takes no WIT argument.
  calls.toString(1);
  const services: [
    acquisition.AcquisitionOptions_,
    context.Context_,
    dynamic.DynamicReference_,
    promise.Promise_,
    base.ReferenceBase_,
  ] = await Promise.all([
    acquisition.get(scope, address),
    context.get(scope, address),
    dynamic.get(scope, address),
    promise.get(scope, address),
    base.get(scope, address),
  ]);
  const sum: number = await services[1].add(1, ctx);
  return { made, text, owned, wrapped, services, sum };
}
`;

test("a generated client type-checks for WIT names TypeScript reads otherwise", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
  );
  try {
    await writeFile(path.join(root, "contract.wit"), NAMES);
    const out = path.join(root, "clients");
    const files = await client({ wit: path.join(root, "contract.wit"), world: "app", out });
    // In an interface, `new(..)` would declare a construct signature; the quoted name is a method.
    expect(await readFile(path.join(out, "example-names-calls.ts"), "utf8")).toContain(
      '  "new"(value: number, context?: Context): Promise<number>;',
    );
    // A WIT type takes a `_` where the module or its interface already uses its name.
    expect(await readFile(path.join(out, "example-names-calls.ts"), "utf8")).toContain(
      "  wrap(value: Calls_, scope: Context_, context?: Context): Promise<Calls_>;",
    );
    // An interface's type takes a `_` where the module already uses its name.
    for (const [file, service] of [
      ["acquisition-options", "AcquisitionOptions_"],
      ["context", "Context_"],
      ["dynamic-reference", "DynamicReference_"],
      ["promise", "Promise_"],
      ["reference-base", "ReferenceBase_"],
    ]) {
      const generated = await readFile(path.join(out, `example-names-${file}.ts`), "utf8");
      expect(generated).toContain(`export interface ${service} {`);
      expect(generated).toContain("  add(value: number, context?: Context): Promise<number>;");
    }
    const site = path.join(out, "site.ts");
    await writeFile(site, NAMES_SITE);
    const reported = diagnostics([...files, site]);
    expect([...reported].filter(([, messages]) => messages.length)).toEqual([]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

/** Types an interface takes with `use`, one renamed, and a type of another interface inside one. */
const USED = (extra: string) => `package example:used@1.0.0;
interface a { record point { x: u32 } record holder { at: point } }
interface b { record point { y: string } record shape { at: point } }
interface s {
  use a.{point};
  use b.{point as spot};
  ${extra}
  f: func(p: spot);
  g: func(p: point);
}
interface u { use a.{holder}; k: func(h: holder); }
interface other { record point { z: bool } m: func(p: point); }
interface both { use a.{holder}; use b.{shape}; n: func(h: holder, s: shape); }
interface fields { use a.{holder}; use b.{shape}; record a-point { w: u8 } r: func(h: holder, s: shape, p: a-point); }
interface renamed { use a.{holder}; use b.{shape}; use b.{point as a-point}; r: func(h: holder, s: shape, p: a-point); }
world app { export s; export u; export other; export both; }
world record-clash { export fields; }
world alias-clash { export renamed; }
`;

test("a generated client names the types its interface takes by their names there", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
  );
  try {
    const generate = async (extra: string) => {
      await writeFile(path.join(root, "contract.wit"), USED(extra));
      const out = path.join(root, "clients");
      await rm(out, { recursive: true, force: true });
      await client({ wit: path.join(root, "contract.wit"), world: "app", out });
      const read = (name: string) => readFile(path.join(out, `example-used-${name}.ts`), "utf8");
      return {
        s: await read("s"),
        u: await read("u"),
        other: await read("other"),
        both: await read("both"),
      };
    };
    // The alias names the type it takes; a function rendered first changes no name.
    for (const extra of ["", "e: func(x: point);", "a0: func(x: point, y: spot);"]) {
      const { s } = await generate(extra);
      expect(s).toContain("export type Point = { x: number };");
      expect(s).toContain("export type Spot = { y: string };");
      expect(s).toContain("  f(p: Spot, context?: Context): Promise<void>;");
      expect(s).toContain("  g(p: Point, context?: Context): Promise<void>;");
      expect(s).not.toContain("Point_");
    }
    const { u, other, both } = await generate("");
    // A named type reached only inside another type's declaration is exported too.
    expect(u).toContain("export type Holder = { at: Point };");
    expect(u).toContain("export type Point = { x: number };");
    // Two interfaces with one type name each have their own, in their own module.
    expect(other).toContain("export type Point = { z: boolean };");
    // Two different types of one name in one module are named after the interface of each.
    expect(both).toContain("export type APoint = { x: number };");
    expect(both).toContain("export type BPoint = { y: string };");
    expect(both).toContain("export type Holder = { at: APoint };");
    expect(both).toContain("export type Shape = { at: BPoint };");
    expect(both).not.toMatch(/export type Point\b/);
    // Two different types left with one name after that are refused, each named with the
    // interface where its name is written.
    await expect(
      client({ wit: path.join(root, "contract.wit"), world: "record-clash", out: root }),
    ).rejects.toThrow(
      "example:used/fields@1.0.0: the WIT types a-point of example:used/fields@1.0.0 and " +
        "point of example:used/a@1.0.0 both name APoint",
    );
    await expect(
      client({ wit: path.join(root, "contract.wit"), world: "alias-clash", out: root }),
    ).rejects.toThrow(
      "example:used/renamed@1.0.0: the WIT types a-point of example:used/renamed@1.0.0 and " +
        "point of example:used/a@1.0.0 both name APoint",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

const fixtures = process.env.TEMBRA_WASM_FIXTURES;
const MAX = (1n << 64n) - 1n;
const MIN = -(1n << 63n);
const SMAX = (1n << 63n) - 1n;

test.skipIf(!fixtures)(
  "Node: a generated client calls a source Orb with the host's values",
  async () => {
    const root = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-client-"),
    );
    const engine = process.env.TEMBRA_WASM_ENGINE;
    const host = createHost(
      engine ? { isolated: { engine: new Uint8Array(await readFile(engine)) } } : {},
    );
    try {
      const [file] = await client({
        wit: path.join(sdk, "test/fixtures/wasm-precision/contract.wit"),
        world: "provider",
        out: root,
      });
      const numbers = (await import(file!)) as {
        get: (scope: unknown, address: { orb: string; key: string }) => Promise<any>;
      };
      await host.install({
        artifact: new Uint8Array(await readFile(path.join(fixtures!, "precision.wasm"))),
      });
      const direct = await numbers.get(host, { orb: "precision", key: "main" });
      try {
        expect(await direct.add(MAX - 1n, 1n)).toBe(MAX);
        const sample = { id: 1n, values: [MIN, -1n, 0n, SMAX], maybe: null, small: 7, label: "x" };
        expect(await direct.echo(sample, { tag: "signed", val: SMAX }, [MAX, "t"])).toEqual({
          tag: "ok",
          val: { id: MAX, values: [SMAX, 0n, -1n, MIN], maybe: SMAX, small: 7, label: "x:t" },
        });
        expect(await direct.echo(sample, { tag: "signed", val: -5n }, [0n, ""])).toEqual({
          tag: "err",
          val: -5n,
        });
        const scoped = await host.root(async (context) => {
          const inner = await numbers.get(context, { orb: "precision", key: "main" });
          return inner.add(2n, 3n, context);
        });
        expect(scoped).toBe(5n);
        // A context forwarded as `undefined` is left out, not sent as a third argument.
        expect(await direct.add(2n, 3n, undefined)).toBe(5n);
      } finally {
        direct.reference.release();
      }
    } finally {
      await host.close();
      await rm(root, { recursive: true, force: true });
    }
    expect(host.diagnostics().references).toBe(0);
  },
);
