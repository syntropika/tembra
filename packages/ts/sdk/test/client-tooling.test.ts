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
import { get, IDENTITY, type Shapes } from "./example-values-shapes.ts";

export async function check(host: Host, context: Context) {
  const identity: "example:values/shapes@1.0.0" = IDENTITY;
  const fromHost: Shapes = await get(host, { orb: "values", key: "main" });
  const fromContext = await get(context, { orb: "values", key: ["team", "a"] }, { principal: "p" });
  const total: bigint = await fromHost.numbers(1, 2.5, 3, 4n);
  const withContext: bigint = await fromHost.numbers(1, 2.5, 3, 4n, context);
  const bytes: number[] = await fromHost.collect([{ name: "a", count: null, bigNumber: 1n }]);
  const tagged = await fromHost.tagged(
    { tag: "named", val: { name: "a", count: 2, bigNumber: 3n } },
    "fast",
    { writeAll: true },
  );
  const count: number | null = tagged.tag === "ok" ? tagged.val.count : tagged.val.length;
  const char: string = await fromHost.deep([[1], []], [5n, "x"]);
  const nothing: void = await fromHost.nothing();
  const maybe: { name: string; count: number | null; bigNumber: bigint } | null =
    await fromHost.maybe(null);
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
  // @ts-expect-error A result stays tagged, never unwrapped.
  const unwrapped: { name: string } = await fromHost.tagged({ tag: "none" }, "fast", {});
  return { identity, total, withContext, bytes, count, char, nothing, maybe, plain, flag, checked, unwrapped };
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

test("a generated client types every WIT kind in the host's value form and checks call sites", async () => {
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
    expect(generated).toContain(
      "collect(values: { name: string; count: number | null; bigNumber: bigint }[], context?: Context): Promise<number[]>;",
    );
    expect(generated).toContain(
      'tagged(value: { tag: "none" } | { tag: "size"; val: number } | { tag: "named"; val: { name: string; count: number | null; bigNumber: bigint } }, mode: "fast" | "slow", access: { read?: boolean; writeAll?: boolean }, context?: Context): Promise<{ tag: "ok"; val: { name: string; count: number | null; bigNumber: bigint } } | { tag: "err"; val: string }>;',
    );
    expect(generated).toContain(
      "deep(value: number[][], pair: [bigint, string], context?: Context): Promise<string>;",
    );
    expect(generated).toContain("nothing(context?: Context): Promise<void>;");
    expect(generated).toContain(
      'plain(context?: Context): Promise<{ tag: "ok" } | { tag: "err" }>;',
    );
    expect(generated).toContain(
      'kebabName(firstValue: number, delete_: boolean, context_: ({ tag: "none" } | { tag: "size"; val: number } | { tag: "named"; val: { name: string; count: number | null; bigNumber: bigint } })[], context?: Context): Promise<boolean>;',
    );
    expect(generated).toContain('kebabName: (...args) => reference["kebab-name"](...args),');
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
