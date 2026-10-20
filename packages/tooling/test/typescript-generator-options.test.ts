import { test } from "bun:test";
import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { Type, local, method, native } from "@tembra/sdk";
import ts from "typescript";

import { generateTypeScript } from "../src/typescript-generator.js";

test("native mapping mutation after admission cannot change the generated consumer signature", async () => {
  const contract = local("generated.captured", {
    version: 1,
    input: Type.Object({}),
    members: { echo: method(native<number>("number@1"), native<number>("number@1")) },
  });
  const nativeTypes = { "number@1": "number" };
  const options = { name: "Captured", nativeTypes };
  const admitted = generateTypeScript(contract.descriptor, options);
  nativeTypes["number@1"] = "string";
  options.name = "Changed";
  const generated = await admitted;
  const directory = await mkdtemp(
    fileURLToPath(new URL("../node_modules/.tembra-capture-", import.meta.url)),
  );
  try {
    await writeFile(path.join(directory, "package.json"), '{"type":"module"}');
    const file = path.join(directory, "generated.ts");
    await writeFile(file, generated);
    const usage = path.join(directory, "usage.ts");
    await writeFile(
      usage,
      [
        'import type { CapturedReference } from "./generated.js";',
        "declare const reference: CapturedReference;",
        "const result: Promise<number> = reference.echo(1);",
        "void result;",
      ].join("\n"),
    );
    const program = ts.createProgram([file, usage], {
      strict: true,
      target: ts.ScriptTarget.ES2022,
      module: ts.ModuleKind.NodeNext,
      moduleResolution: ts.ModuleResolutionKind.NodeNext,
      noEmit: true,
      skipLibCheck: true,
      types: [],
    });
    const diagnostics = ts.getPreEmitDiagnostics(program);
    assert.equal(
      diagnostics.length,
      0,
      ts.formatDiagnostics(diagnostics, {
        getCurrentDirectory: () => directory,
        getCanonicalFileName: (filename) => filename,
        getNewLine: () => "\n",
      }),
    );
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
