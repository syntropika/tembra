import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { Type, hostNamed, local, method, native, remote } from "@tembra/sdk";
import type { BindingPort } from "@tembra/sdk";
import ts from "typescript";

import { generateTypeScript } from "../../src/tooling/typescript-generator.js";

const corpus: unknown = JSON.parse(
  await readFile(
    new URL("../../../../testing/fixtures/contracts/v1.json", import.meta.url),
    "utf-8",
  ),
);
assert.ok(
  corpus !== null &&
    typeof corpus === "object" &&
    "descriptors" in corpus &&
    Array.isArray(corpus.descriptors),
);
const descriptors: readonly unknown[] = corpus.descriptors;
const first = descriptors.at(0);
assert.ok(first !== null && typeof first === "object" && "descriptor" in first);
const fixtureDescriptor = first.descriptor;

async function generatedReference(
  url: URL,
  name: string,
  port: BindingPort,
): Promise<{ increment: (args: { by: number }) => Promise<unknown> }> {
  const namespace: unknown = await import(url.href);
  assert.ok(namespace !== null && typeof namespace === "object");
  const factory: unknown = Reflect.get(namespace, name);
  assert.equal(typeof factory, "function");
  if (typeof factory !== "function") {
    throw new TypeError("Generated binding factory is missing");
  }
  const reference: unknown = Reflect.apply(factory, namespace, [port]);
  assert.ok(reference !== null && typeof reference === "object");
  const increment: unknown = Reflect.get(reference, "increment");
  if (typeof increment !== "function") {
    throw new TypeError("Generated increment operation is missing");
  }
  return {
    increment: async (args) => {
      const result: unknown = Reflect.apply(increment, reference, [args]);
      return await Promise.resolve(result);
    },
  };
}

async function compileGenerated(
  generated: string,
  usageSource: string,
  exercise: (url: URL) => Promise<void>,
): Promise<void> {
  const root = fileURLToPath(new URL("../..", import.meta.url));
  const directory = await mkdtemp(path.resolve(root, "node_modules/.tembra-generator-"));
  try {
    await mkdir(path.join(directory, "node_modules/@tembra"), { recursive: true });
    await symlink(root, path.join(directory, "node_modules/@tembra/sdk"), "dir");
    await writeFile(path.resolve(directory, "package.json"), '{"type":"module"}');
    const file = path.resolve(directory, "generated.ts");
    await writeFile(file, generated);
    const usage = path.resolve(directory, "usage.ts");
    await writeFile(usage, usageSource);
    const program = ts.createProgram([file, usage], {
      strict: true,
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.NodeNext,
      moduleResolution: ts.ModuleResolutionKind.NodeNext,
      noEmit: true,
      skipLibCheck: false,
      types: [],
    });
    const diagnostics = ts.getPreEmitDiagnostics(program);
    assert.equal(
      diagnostics.length,
      0,
      ts.formatDiagnosticsWithColorAndContext(diagnostics, {
        getCurrentDirectory: () => directory,
        getCanonicalFileName: (canonicalFile) => canonicalFile,
        getNewLine: () => "\n",
      }),
    );
    await exercise(pathToFileURL(file));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test("verified descriptor generates DTOs, typed variant methods, and executable binding metadata", async () => {
  const generated = await generateTypeScript(fixtureDescriptor, {
    name: "Counter",
  });
  assert.match(generated, /export interface CounterInput/u);
  assert.match(generated, /readonly "increment": \(args:/u);
  assert.match(generated, /readonly "read": \(context\?:/u);
  assert.doesNotMatch(generated, /\bany\b/u);
  await compileGenerated(
    generated,
    `import {Counter,bindCounter} from './generated.js';\nimport type {BindingPort,InputOf} from '@tembra/sdk';\nconst input:InputOf<typeof Counter>={name:'counter'};\n// @ts-expect-error: acquisition input has a required name\nconst bad:InputOf<typeof Counter>={};\nfunction types(port:BindingPort){ const ref=bindCounter(port); if(ref.variant==='default'){\nvoid ref.increment({by:1}); void ref.nullable(null); void ref.read();\n// @ts-expect-error: argument must be a number\nvoid ref.increment({by:'bad'});\n// @ts-expect-error: nullable argument is present\nvoid ref.nullable();\n// @ts-expect-error: no argument\nvoid ref.read(3);\n}}\nvoid types;void input;void bad;\n`,
    async (url) => {
      const port: BindingPort = {
        status: "live",
        variant: "default",
        shape: undefined,
        error: undefined,
        ready: Promise.resolve(),
        async call(request) {
          assert.equal(request.member, "increment");
          assert.deepEqual(request.args, { present: true, value: { by: 4 } });
          return { present: true, value: 5 };
        },
        readable() {
          return {
            value: 0,
            subscribe() {
              return () => {};
            },
          };
        },
        opaque() {
          throw new Error("no opaque");
        },
        release() {},
        onStatus() {
          return () => {};
        },
        onShape() {
          return () => {};
        },
      };
      const reference = await generatedReference(url, "bindCounter", port);
      assert.equal(await reference.increment({ by: 4 }), 5);
    },
  );
});

test("TypeBox-origin descriptor generates the same typed consumer binding", async () => {
  const contract = remote("generated.notes", {
    version: 1,
    input: Type.Object({ session: Type.String() }),
    host: hostNamed("server"),
    variants: {
      view: {
        state: { notes: Type.Array(Type.String()) },
        methods: { add: method(Type.String()) },
      },
    },
  });
  const generated = await generateTypeScript(contract.descriptor, {
    name: "Notes",
  });
  assert.match(generated, /Readable<NotesViewNotesState>/u);
  assert.match(generated, /Promise<undefined>/u);
  assert.match(generated, /"view" \| undefined/u);
});

test("a Serde/Schemars-origin contract produces a typed reference without rewriting its schema", async () => {
  const descriptor: unknown = JSON.parse(
    await readFile(
      new URL("../../../../testing/fixtures/contracts/typed-rust.json", import.meta.url),
      "utf-8",
    ),
  );
  const generated = await generateTypeScript(descriptor, {
    name: "TypedCounter",
  });
  assert.match(generated, /export interface TypedCounterInput/u);
  assert.match(generated, /name: string/u);
  assert.match(generated, /by: number/u);
  assert.match(generated, /26c3d3f912aa233a48750cc691248a92e3b545f94f97bf1f94cf035c282547b8/u);
  await compileGenerated(
    generated,
    `import {TypedCounter,bindTypedCounter} from './generated.js';\nimport type {BindingPort,InputOf} from '@tembra/sdk';\nconst input:InputOf<typeof TypedCounter>={name:'counter'};\nfunction types(port:BindingPort){const ref=bindTypedCounter(port);if(ref.variant==='default'){const n:number=ref.count.value;void n;void ref.increment({by:1});\n// @ts-expect-error: generated Rust struct field is numeric\nvoid ref.increment({by:'wrong'});\n}}\nvoid input;void types;\n`,
    async (url) => {
      const port: BindingPort = {
        status: "live",
        variant: "default",
        shape: undefined,
        error: undefined,
        ready: Promise.resolve(),
        async call(request) {
          assert.deepEqual(request.args, { present: true, value: { by: 4 } });
          return { present: true, value: 5 };
        },
        readable() {
          return {
            value: 0,
            subscribe() {
              return () => {};
            },
          };
        },
        opaque() {
          throw new Error("no opaque");
        },
        release() {},
        onStatus() {
          return () => {};
        },
        onShape() {
          return () => {};
        },
      };
      const reference = await generatedReference(url, "bindTypedCounter", port);
      assert.equal(await reference.increment({ by: 4 }), 5);
      await assert.rejects(reference.increment({ by: 2_147_483_648 }), /validation/u);
    },
  );
});

test("unmapped native capabilities fail rather than inventing a portable DTO", async () => {
  const contract = local("generated.native", {
    version: 1,
    input: Type.Object({}),
    members: { echo: method(native<URL>("url@1"), native<URL>("url@1")) },
  });
  await assert.rejects(generateTypeScript(contract.descriptor), /native_mapping/u);
  const generated = await generateTypeScript(contract.descriptor, {
    nativeTypes: { "url@1": "URL" },
  });
  assert.match(generated, /args: URL, context\?: ContextHandle\) => Promise<URL>/u);
  await assert.rejects(generateTypeScript(contract.descriptor, { name: "class" }), /nonreserved/u);
});

test("independent DTOs with repeated nested annotation titles do not collide", async () => {
  const contract = remote("generated.repeated-titles", {
    version: 1,
    input: Type.Object({}),
    host: hostNamed("server"),
    variants: {
      main: {
        methods: {
          text: method(
            Type.Object({
              nested: Type.Object({ value: Type.String() }, { title: "Payload" }),
            }),
          ),
          number: method(
            Type.Object({
              nested: Type.Object({ value: Type.Integer() }, { title: "Payload" }),
            }),
          ),
        },
      },
    },
  });
  const generated = await generateTypeScript(contract.descriptor, {
    name: "Repeated",
  });
  await compileGenerated(
    generated,
    `import {bindRepeated} from './generated.js';\nimport type {BindingPort} from '@tembra/sdk';\nfunction types(port:BindingPort){const ref=bindRepeated(port);if(ref.variant==='main'){void ref.text({nested:{value:'text'}});void ref.number({nested:{value:1}});\n// @ts-expect-error: repeated title does not erase the different DTO value type\nvoid ref.number({nested:{value:'wrong'}});}}\nvoid types;\n`,
    async () => {},
  );
});
