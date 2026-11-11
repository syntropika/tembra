import { expect, test } from "bun:test";
import { access, mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import ts from "typescript";
import { NodeArtifact } from "@syntropika/tembra-engine-node";
import { build } from "../src/tooling/index.ts";
import { typecheck } from "../src/tooling/typecheck.ts";

const contract = `package example:authoring@1.0.0;
interface arithmetic { double: func(value: u64) -> u64; }
interface calculation {
  record input { amount: u64, label: string }
  record output { total: u64, label: string }
  apply: async func(input: input) -> output;
  read: func() -> u64;
}
world app { import arithmetic; export calculation; }`;
const guest = JSON.stringify(path.resolve(import.meta.dir, "../src/guest.ts"));
const implementation = `import {orb, type Declaration, type WitMethods} from ${guest};
import type {calculation} from "./contract.wit";
import type * as Arithmetic from "example:authoring/arithmetic@1.0.0";
function create(self: Declaration) {
  return {total: self.state(0n), arithmetic: self.get<typeof Arithmetic>("example:authoring/arithmetic@1.0.0")};
}
export default orb({name:"typed",apiVersion:1,services:{calculation:{create, methods:{
  async apply(environment, context, input) {
    context.check();
    const doubled = await environment.arithmetic.double(input.amount, context);
    environment.total.update(context, () => doubled);
    return {total: environment.total.value, label: input.label};
  },
  read(environment) { return environment.total.value; }
} satisfies WitMethods<typeof calculation, ReturnType<typeof create>>}}});`;

test("WIT contextually infers complete source methods and canonical imports in builds and editors", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-authoring-"),
  );
  try {
    await writeFile(path.join(root, "contract.wit"), contract);
    await writeFile(path.join(root, "orb.ts"), implementation);
    const prepare = () =>
      build({ root, builds: { typed: { entry: "orb.ts", wit: "contract.wit", world: "app" } } });
    const [artifact] = await prepare();
    const decoded = new NodeArtifact(new Uint8Array(await readFile(artifact!.path)));
    expect(JSON.parse(decoded.description())).toMatchObject({ name: "typed", kind: "source" });
    // Reopen exactly the persisted editor declarations without the build's virtual filesystem.
    expect(() => typecheck(path.join(root, "orb.ts"), root)).not.toThrow();
    const alias = path.join(root, "contract.d.wit.ts");
    const before = await stat(alias, { bigint: true });
    await prepare();
    expect((await stat(alias, { bigint: true })).mtimeNs).toBe(before.mtimeNs);
    for (const [from, to, message] of [
      ["input.amount, context", "input.label, context", "not assignable"],
      ["label: input.label", "label: input.amount", "not assignable"],
      ["context.check();", "context.nonexistent();", "does not exist"],
      ["environment.total.update", "environment.nonexistent.update", "does not exist"],
      ["  read(environment) { return environment.total.value; }", "", "read"],
      ["  read(environment)", "  extra() { return 0n; }, read(environment)", "extra"],
      [
        'example:authoring/arithmetic@1.0.0";',
        'example:authoring/missing@1.0.0";',
        "Cannot find module",
      ],
    ]) {
      await writeFile(path.join(root, "orb.ts"), implementation.replace(from!, to!));
      await expect(prepare()).rejects.toThrow(message);
    }
    await writeFile(path.join(root, "orb.ts"), implementation);
    await writeFile(
      path.join(root, "contract.wit"),
      contract.replace("double: func(value: u64)", "double: func(value: string)"),
    );
    await expect(prepare()).rejects.toThrow("not assignable");
    await expect(() => typecheck(path.join(root, "orb.ts"), root)).toThrow("not assignable");
    await writeFile(
      path.join(root, "contract.wit"),
      contract.replace("amount: u64", "amount: string"),
    );
    await expect(prepare()).rejects.toThrow("not assignable");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("one strict editor project shares canonical imports across different contracts and rejects conflicts", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-shared-types-"),
  );
  const shared = (name: string) => `package example:shared@1.0.0;
    interface data { record payload { value: u64 } }
    interface arithmetic {
      use data.{payload};
      record input { amount: u64, payload: payload, outcome: result<u64, string> }
      resource session { constructor(); read: func() -> u64; }
      double: func(value: input) -> u64;
    }
    interface ${name} { run: func() -> u64; }
    world app { import arithmetic; export ${name}; }`;
  try {
    for (const name of ["alpha", "beta"]) {
      await mkdir(path.join(root, name));
      await writeFile(path.join(root, name, "contract.wit"), shared(name));
      await writeFile(
        path.join(root, name, "orb.ts"),
        `export default {name:${JSON.stringify(name)}, apiVersion:1,services:{${name}:{methods:{run:()=>1n}}}} as const;`,
      );
    }
    const prepare = () =>
      build({
        root,
        builds: {
          alpha: { entry: "alpha/orb.ts", wit: "alpha/contract.wit", world: "app" },
          beta: { entry: "beta/orb.ts", wit: "beta/contract.wit", world: "app" },
        },
      });
    expect(await prepare()).toHaveLength(2);
    const entry = path.join(root, "editor.ts");
    const source = `import type {alpha} from "./alpha/contract.wit";
      import type {beta} from "./beta/contract.wit";
      import type * as Arithmetic from "example:shared/arithmetic@1.0.0";
      const input: Arithmetic.Input = {amount:1n,payload:{value:2n},outcome:{tag:"ok",val:3n}};
      const outcome: Arithmetic.Result<bigint,string> = {tag:"ok",val:4n};
      function inspect(session: Arithmetic.Session): bigint {
        session[Symbol.dispose]();
        return session.read();
      }
      const total: ReturnType<typeof Arithmetic.double> = inspect({} as Arithmetic.Session);
      const constructor: typeof Arithmetic.Session | undefined = undefined;`;
    await writeFile(entry, source);
    const diagnostics = () => {
      const project = ts.parseJsonConfigFileContent({ include: ["**/*.ts"] }, ts.sys, root);
      return ts.getPreEmitDiagnostics(
        ts.createProgram(project.fileNames, {
          target: ts.ScriptTarget.ES2023,
          module: ts.ModuleKind.ESNext,
          moduleResolution: ts.ModuleResolutionKind.Bundler,
          strict: true,
          noEmit: true,
          allowArbitraryExtensions: true,
          skipLibCheck: false,
          types: [],
          lib: ["lib.es2023.d.ts", "lib.esnext.disposable.d.ts"],
        }),
      );
    };
    expect(diagnostics()).toEqual([]);
    await writeFile(entry, source.replace("amount:1n", "amount:1"));
    expect(diagnostics().map((diagnostic) => diagnostic.code)).toEqual([2322]);
    await writeFile(entry, source.replace("session.read()", "session.read().toUpperCase()"));
    expect(diagnostics().map((diagnostic) => diagnostic.code)).toEqual([2339]);
    await writeFile(entry, source.replace("val:4n", 'val:"wrong"'));
    expect(diagnostics().map((diagnostic) => diagnostic.code)).toEqual([2322]);
    const alias = path.join(root, "beta/contract.d.wit.ts");
    const before = await readFile(alias, "utf8");
    await writeFile(
      path.join(root, "beta/contract.wit"),
      shared("beta").replace("value: u64", "value: string"),
    );
    await expect(prepare()).rejects.toThrow("Conflicting WIT declarations for example:shared/");
    expect(await readFile(alias, "utf8")).toBe(before);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("ambiguous generated versions are rejected before publishing editor aliases", async () => {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-versioned-types-"),
  );
  try {
    await writeFile(
      path.join(root, "contract.wit"),
      `package example:versions@1.0.0;
      interface output { run:func(); }
      world app {
        import first: example:shared/arithmetic@1.0.0;
        import second: example:shared/arithmetic@2.0.0;
        export output;
      }`,
    );
    for (const version of [1, 2]) {
      const directory = path.join(root, "deps", `shared${version}`);
      await mkdir(directory, { recursive: true });
      await writeFile(
        path.join(directory, "shared.wit"),
        `package example:shared@${version}.0.0;
        interface arithmetic { double:func(value:${version === 1 ? "u64" : "string"}) -> ${version === 1 ? "u64" : "string"}; }`,
      );
    }
    await writeFile(
      path.join(root, "orb.ts"),
      `export default {name:"versions",apiVersion:1,services:{output:{methods:{run(){}}}}} as const;`,
    );
    await expect(
      build({ root, builds: { versions: { entry: "orb.ts", wit: "contract.wit", world: "app" } } }),
    ).rejects.toThrow("Ambiguous generated WIT interface");
    await expect(access(path.join(root, "contract.d.wit.ts"))).rejects.toThrow();
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
