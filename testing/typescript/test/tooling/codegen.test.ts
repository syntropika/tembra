import { test } from "bun:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { setTimeout as delay } from "node:timers/promises";

import { Type, canonicalJson, local, method } from "@tembra/sdk";
import { generateBindings, resolveConfig, ToolingError } from "@tembra/sdk/tooling";
import ts from "typescript";

const Contract = local("tooling.counter", {
  version: 1,
  input: Type.Object({}),
  members: { read: method(undefined, Type.Integer()) },
});

function countedToken(countKey: string, hookKey?: string): string {
  return `globalThis[${JSON.stringify(countKey)}] = (globalThis[${JSON.stringify(countKey)}] ?? 0) + 1;
    export const Counter = { get descriptor() { ${hookKey === undefined ? "" : `globalThis[${JSON.stringify(hookKey)}]?.();`}
      return ${JSON.stringify(Contract.descriptor)}; } };`;
}

test("descriptor files generate canonical TypeScript without rewrites and repair deleted outputs", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-codegen-"));
  try {
    await writeFile(path.join(root, "contract.json"), JSON.stringify(Contract.descriptor));
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          {
            source: { descriptor: "contract.json" },
            descriptor: "generated/canonical.json",
            typescript: "generated/contract.ts",
            name: "Counter",
          },
        ],
      },
      root,
    );
    const first = await generateBindings(config);
    const typescript = path.join(root, "generated/contract.ts");
    const before = await stat(typescript, { bigint: true });
    assert.match(await readFile(typescript, "utf-8"), /export const Counter = fromDescriptor/u);
    assert.equal(
      await readFile(path.join(root, "generated/canonical.json"), "utf-8"),
      `${canonicalJson(Contract.descriptor)}\n`,
    );
    assert.deepEqual(first.inputs, [path.join(root, "contract.json")]);
    await generateBindings(config);
    const after = await stat(typescript, { bigint: true });
    assert.equal(after.mtimeNs, before.mtimeNs);
    await rm(typescript);
    await generateBindings(config);
    assert.match(await readFile(typescript, "utf-8"), /CounterReference/u);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("TypeScript token modules refresh local dependency changes and feed Rust generator stdin", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-module-"));
  try {
    const dependency = path.join(root, "descriptor.ts");
    await writeFile(
      dependency,
      `export const descriptor = ${JSON.stringify(Contract.descriptor)};`,
    );
    await writeFile(
      path.join(root, "token.ts"),
      'import { descriptor } from "./descriptor.js"; export const Counter = { descriptor };',
    );
    const generator = path.join(root, "rust-generator.ts");
    await writeFile(
      generator,
      'let source = ""; for await (const chunk of process.stdin) source += chunk; console.log("// " + JSON.parse(source).name);',
    );
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          {
            source: { module: "token.ts", export: "Counter" },
            rust: "generated/counter.rs",
            rustCommand: [process.execPath, generator],
            typescript: "generated/counter.ts",
          },
        ],
      },
      root,
    );
    const result = await generateBindings(config);
    assert.ok(result.inputs.includes(dependency));
    assert.equal(
      await readFile(path.join(root, "generated/counter.rs"), "utf-8"),
      "// tooling.counter\n",
    );
    await writeFile(
      dependency,
      `export const descriptor = ${JSON.stringify(local("tooling.changed", { version: 1, input: Type.Object({}), members: { read: method(undefined, Type.Integer()) } }).descriptor)};`,
    );
    await generateBindings(config);
    assert.equal(
      await readFile(path.join(root, "generated/counter.rs"), "utf-8"),
      "// tooling.changed\n",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("argv descriptor exporters preserve arguments and original failure details", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-command-"));
  try {
    const script = path.join(root, "descriptor exporter.ts");
    const watch = path.join(root, "contract.json");
    await writeFile(watch, JSON.stringify(Contract.descriptor));
    await writeFile(
      script,
      'import { readFileSync } from "node:fs"; process.stdout.write(readFileSync(process.argv[2], "utf8"));',
    );
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          {
            source: {
              command: [process.execPath, script, watch],
              watch: ["contract.json", "descriptor exporter.ts"],
            },
            typescript: "out.ts",
          },
        ],
      },
      root,
    );
    const result = await generateBindings(config);
    assert.deepEqual(result.inputs, [watch, script, process.execPath].toSorted());
    await writeFile(script, 'console.error("export failure detail"); process.exit(7);');
    await assert.rejects(
      generateBindings(config),
      (error) =>
        error instanceof ToolingError &&
        error.phase === "codegen" &&
        error.cause instanceof Error &&
        error.cause.message.includes("export failure detail"),
    );
    assert.match(await readFile(path.join(root, "out.ts"), "utf-8"), /tooling.counter/u);
    const controller = new AbortController();
    controller.abort(new Error("stopped"));
    await assert.rejects(
      generateBindings(config, { signal: controller.signal }),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "cancelled",
    );
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("actual compiled Rust descriptor export generates TypeScript and TypeScript descriptor generates Rust", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-real-codegen-"));
  const repository = path.resolve(import.meta.dirname, "../../../..");
  try {
    const exporter = path.join(repository, "target/debug/tembra-testing-emit-contract");
    const generator = path.join(repository, "target/debug/tembra-codegen");
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          {
            source: { command: [exporter], watch: [] },
            typescript: "generated/rust-counter.ts",
            name: "RustCounter",
          },
          {
            source: { module: "typescript-contract.ts", export: "Counter" },
            rust: "generated/typescript-counter.rs",
            rustCommand: [generator],
          },
        ],
      },
      root,
    );
    await mkdir(path.join(root, "node_modules/@tembra"), { recursive: true });
    await symlink(
      path.join(repository, "packages/sdk"),
      path.join(root, "node_modules/@tembra/sdk"),
      "dir",
    );
    await writeFile(
      path.join(root, "typescript-contract.ts"),
      'import { local, Type, method } from "@tembra/sdk"; export const Counter = local("tooling.counter", { version: 1, input: Type.Object({}), members: { read: method(undefined, Type.Integer()) } });',
    );
    await generateBindings(config);
    assert.match(
      await readFile(path.join(root, "generated/rust-counter.ts"), "utf-8"),
      /fixture.typed-counter/u,
    );
    const rust = await readFile(path.join(root, "generated/typescript-counter.rs"), "utf-8");
    assert.match(rust, /tooling.counter/u);
    assert.match(rust, /pub (?:struct|trait)/u);
    const program = ts.createProgram([path.join(root, "generated/rust-counter.ts")], {
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
      ts.formatDiagnostics(diagnostics, {
        getCurrentDirectory: () => root,
        getCanonicalFileName: (filename) => filename,
        getNewLine: () => "\n",
      }),
    );
    await writeFile(
      path.join(root, "Cargo.toml"),
      [
        "[workspace]",
        "[package]",
        'name = "typescript-descriptor-consumer"',
        'version = "0.0.0"',
        'edition = "2024"',
        "[[bin]]",
        'name = "consumer"',
        'path = "main.rs"',
        "[dependencies]",
        `tembra = { path = ${JSON.stringify(path.join(repository, "crates/tembra"))} }`,
      ].join("\n"),
    );
    await writeFile(
      path.join(root, "main.rs"),
      '#![allow(dead_code)]\nmod generated { include!("generated/typescript-counter.rs"); }\nfn main() {}\n',
    );
    const compiled = spawnSync(
      "cargo",
      [
        "check",
        "--offline",
        "--quiet",
        "--manifest-path",
        path.join(root, "Cargo.toml"),
        "--target-dir",
        path.join(repository, "target/codegen-consumer-tests"),
      ],
      { encoding: "utf-8" },
    );
    assert.equal(compiled.status, 0, compiled.stderr);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test.skipIf(process.platform === "win32")(
  "cancelling an exporter joins its process group, including a child ignoring SIGTERM",
  async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "tembra-cancel-codegen-"));
    try {
      const script = path.join(root, "exporter.ts");
      const pidFile = path.join(root, "child.pid");
      const childSource =
        'process.on("SIGTERM", () => {}); require("node:fs").writeFileSync(process.argv[1], String(process.pid)); setInterval(() => {}, 1000)';
      await writeFile(
        script,
        `import { spawn } from "node:child_process"; spawn(process.execPath, ["-e", ${JSON.stringify(childSource)}, process.argv[2]], { stdio: "inherit" }); setInterval(() => {}, 1000);`,
      );
      const config = resolveConfig(
        {
          builds: {},
          codegen: [
            {
              source: { command: [process.execPath, script, pidFile], watch: [script] },
              typescript: "out.ts",
            },
          ],
        },
        root,
      );
      const controller = new AbortController();
      const result = generateBindings(config, { signal: controller.signal });
      const refusal = assert.rejects(
        result,
        (failure: unknown) => failure instanceof ToolingError && failure.phase === "cancelled",
      );
      let pid: number | undefined;
      for (let attempt = 0; attempt < 100; attempt += 1) {
        // oxlint-disable-next-line no-await-in-loop -- Poll for child readiness before cancelling its process group.
        const text = await readFile(pidFile, "utf-8").catch(() => undefined);
        if (text !== undefined) {
          pid = Number(text);
          break;
        }
        // oxlint-disable-next-line no-await-in-loop -- Each readiness probe waits for the child instead of starting independent work.
        await delay(10);
      }
      assert.ok(pid !== undefined, "exporter started its child");
      controller.abort(new Error("stop exporter"));
      await refusal;
      // A reaped child can briefly remain a zombie under an external process supervisor.
      // Its inherited stdout pipe must close before generateBindings can settle.
      assert.equal(
        await readFile(path.join(root, "out.ts"), "utf-8").catch(() => undefined),
        undefined,
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
  5000,
);

test.skipIf(process.platform === "win32")(
  "PATH and default Rust executable replacements invalidate the binding cache",
  async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "tembra-generator-cache-"));
    const previousPath = process.env.PATH;
    try {
      const bin = path.join(root, "bin");
      await mkdir(bin);
      const generator = path.join(bin, "tembra-codegen");
      await writeFile(generator, `#!${process.execPath}\nconsole.log("// first");\n`);
      await chmod(generator, 0o755);
      await symlink(generator, path.join(bin, "custom-generator"));
      process.env.PATH = `${bin}${path.delimiter}${previousPath ?? ""}`;
      await writeFile(path.join(root, "contract.json"), JSON.stringify(Contract.descriptor));
      const config = resolveConfig(
        {
          builds: {},
          codegen: [
            { source: { descriptor: "contract.json" }, rust: "default.rs" },
            {
              source: { descriptor: "contract.json" },
              rust: "custom.rs",
              rustCommand: ["custom-generator"],
            },
          ],
        },
        root,
      );
      const result = await generateBindings(config);
      assert.ok(result.inputs.includes(generator));
      assert.equal(await readFile(path.join(root, "default.rs"), "utf-8"), "// first\n");
      await writeFile(generator, `#!${process.execPath}\nconsole.log("// second");\n`);
      await generateBindings(config);
      assert.equal(await readFile(path.join(root, "default.rs"), "utf-8"), "// second\n");
      assert.equal(await readFile(path.join(root, "custom.rs"), "utf-8"), "// second\n");
    } finally {
      if (previousPath === undefined) {
        delete process.env.PATH;
      } else {
        process.env.PATH = previousPath;
      }
      await rm(root, { recursive: true, force: true });
    }
  },
);

test("all token dependency graphs and tsconfig extends are protected before any generated write", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-codegen-collision-"));
  try {
    const authored = `export const descriptor = ${JSON.stringify(Contract.descriptor)};`;
    await writeFile(path.join(root, "authored.ts"), authored);
    await writeFile(
      path.join(root, "token.ts"),
      'import { descriptor } from "./authored.js"; export const Counter = { descriptor };',
    );
    await writeFile(path.join(root, "contract.json"), JSON.stringify(Contract.descriptor));
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          { source: { descriptor: "contract.json" }, typescript: "authored.ts" },
          { source: { module: "token.ts", export: "Counter" }, typescript: "other.ts" },
        ],
      },
      root,
    );
    await assert.rejects(
      generateBindings(config),
      (error) => error instanceof ToolingError && error.reason === "output_collision",
    );
    assert.equal(await readFile(path.join(root, "authored.ts"), "utf-8"), authored);
    assert.equal(
      await readFile(path.join(root, "other.ts"), "utf-8").catch(() => undefined),
      undefined,
    );
    await writeFile(path.join(root, "base.json"), "{}");
    await writeFile(path.join(root, "tsconfig.json"), '{"extends":"./base.json"}');
    await symlink(root, path.join(root, "alias"), "dir");
    const jsonCollision = resolveConfig(
      {
        builds: {},
        codegen: [{ source: { descriptor: "contract.json" }, typescript: "alias/base.json" }],
      },
      root,
    );
    await assert.rejects(
      generateBindings(jsonCollision),
      (error) => error instanceof ToolingError && error.reason === "output_collision",
    );
    assert.equal(await readFile(path.join(root, "base.json"), "utf-8"), "{}");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("binding generation captures configuration before its first asynchronous boundary", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-codegen-snapshot-"));
  try {
    const source = path.join(root, "contract.json");
    const original = JSON.stringify(Contract.descriptor);
    await writeFile(source, original);
    const config = resolveConfig(
      {
        builds: {},
        codegen: [{ source: { descriptor: "contract.json" }, typescript: "generated.ts" }],
      },
      root,
    );
    const pending = generateBindings(config);
    const binding = config.codegen.at(0);
    assert.ok(binding !== undefined);
    Reflect.set(binding, "typescript", source);
    await pending;
    assert.equal(await readFile(source, "utf-8"), original);
    assert.match(await readFile(path.join(root, "generated.ts"), "utf-8"), /tooling.counter/u);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

for (const exitCode of [0, 23]) {
  test(`generator exit ${exitCode} retains unread-input failure, stderr and previous outputs`, async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "tembra-codegen-pipe-"));
    try {
      const { descriptor } = local("pipe.refusal", {
        version: 1,
        input: Type.Literal("x".repeat(512 * 1024)),
        members: {},
      });
      await writeFile(path.join(root, "contract.json"), JSON.stringify(descriptor));
      await writeFile(path.join(root, "generated.rs"), "previous output");
      const config = resolveConfig(
        {
          builds: {},
          codegen: [
            {
              source: { descriptor: "contract.json" },
              rust: "generated.rs",
              rustCommand: [
                process.execPath,
                "-e",
                `const fs = require('node:fs'); fs.closeSync(0); fs.writeSync(2, 'generator refused before reading input'); process.exit(${exitCode});`,
              ],
            },
          ],
        },
        root,
      );
      await assert.rejects(generateBindings(config), (error: unknown) => {
        assert.ok(error instanceof ToolingError);
        assert.equal(error.phase, "codegen");
        assert.ok(error.cause instanceof Error);
        assert.equal(Reflect.get(error.cause, "code"), exitCode);
        assert.equal(Reflect.get(error.cause, "stderr"), "generator refused before reading input");
        assert.ok(error.cause.cause instanceof Error);
        assert.equal(Reflect.get(error.cause.cause, "code"), "EPIPE");
        return true;
      });
      assert.equal(await readFile(path.join(root, "generated.rs"), "utf-8"), "previous output");
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
}

test("a newly linked output parent cannot overwrite an authored token or run an earlier exporter", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-codegen-output-alias-"));
  try {
    await mkdir(path.join(root, "src"));
    const source = path.join(root, "src/contracts.ts");
    const authored = `export const Counter = { descriptor: ${JSON.stringify(Contract.descriptor)} };`;
    await writeFile(source, authored);
    const config = resolveConfig(
      {
        builds: { one: { entry: "src/contracts.ts", platform: "server" } },
        codegen: [
          {
            source: {
              command: [
                process.execPath,
                "-e",
                `require('node:fs').writeFileSync('export-ran', 'yes');process.stdout.write(${JSON.stringify(JSON.stringify(Contract.descriptor))});`,
              ],
              watch: [],
            },
            descriptor: "generated/contract.json",
          },
          {
            source: { module: "src/contracts.ts", export: "Counter" },
            typescript: "alias/contracts.ts",
          },
        ],
      },
      root,
    );
    await symlink(path.join(root, "src"), path.join(root, "alias"), "dir");
    await assert.rejects(
      generateBindings(config),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
    assert.equal(await readFile(source, "utf-8"), authored);
    const retained = await readdir(root);
    assert.deepEqual(retained.toSorted(), ["alias", "src"]);
    const allowed = resolveConfig(
      {
        builds: {},
        codegen: [
          {
            source: { module: "src/contracts.ts", export: "Counter" },
            typescript: "alias/future/contract.ts",
          },
        ],
      },
      root,
    );
    await generateBindings(allowed);
    assert.match(
      await readFile(path.join(root, "src/future/contract.ts"), "utf-8"),
      /fromDescriptor/u,
    );
    assert.equal(await readFile(source, "utf-8"), authored);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test("aliased token dependencies are refused before any exporter or module evaluation effects", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-codegen-dependency-alias-"));
  try {
    await mkdir(path.join(root, "inputs"));
    await symlink(path.join(root, "inputs"), path.join(root, "alias"), "dir");
    const source = path.join(root, "inputs/authored.ts");
    const authored = `export const descriptor = ${JSON.stringify(Contract.descriptor)};`;
    await writeFile(source, authored);
    await writeFile(
      path.join(root, "token.ts"),
      `import { descriptor } from "./inputs/authored.js"; import { writeFileSync } from "node:fs"; writeFileSync(${JSON.stringify(path.join(root, "module-ran"))}, "yes"); export const Counter = { descriptor };`,
    );
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          {
            source: {
              command: [
                process.execPath,
                "-e",
                `require('node:fs').writeFileSync('export-ran', 'yes');process.stdout.write(${JSON.stringify(JSON.stringify(Contract.descriptor))});`,
              ],
              watch: [],
            },
            descriptor: "generated/contract.json",
          },
          { source: { module: "token.ts", export: "Counter" }, typescript: "alias/authored.ts" },
        ],
      },
      root,
    );
    await assert.rejects(
      generateBindings(config),
      (error: unknown) => error instanceof ToolingError && error.reason === "output_collision",
    );
    assert.equal(await readFile(source, "utf-8"), authored);
    const retained = await readdir(root);
    assert.deepEqual(retained.toSorted(), ["alias", "inputs", "token.ts"]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test.skipIf(process.platform === "win32")(
  "completed exporter snapshots skip unrelated edits and invalidate watch, executable, PATH and output changes",
  async () => {
    const root = await mkdtemp(path.join(os.tmpdir(), "tembra-exporter-cache-"));
    const previousPath = process.env.PATH;
    try {
      await mkdir(path.join(root, "first"));
      await mkdir(path.join(root, "second"));
      const script = `#!${process.execPath}
const fs = require('node:fs'), path = require('node:path');
const counter = path.resolve(__dirname, '../calls.txt');
let count = 0; try { count = Number(fs.readFileSync(counter, 'utf8')); } catch {}
fs.writeFileSync(counter, String(count + 1));
process.stdout.write(fs.readFileSync(path.resolve(__dirname, '../contract.json'), 'utf8'));
`;
      const exporter = path.join(root, "first/emit-contract");
      const alternate = path.join(root, "second/emit-contract");
      await writeFile(exporter, script, { mode: 0o755 });
      await writeFile(alternate, script, { mode: 0o755 });
      await writeFile(path.join(root, "contract.json"), JSON.stringify(Contract.descriptor));
      await writeFile(path.join(root, "facet.ts"), "export default 1;");
      process.env.PATH = `${path.join(root, "first")}${path.delimiter}${previousPath ?? ""}`;
      const config = resolveConfig(
        {
          builds: { main: { entry: "facet.ts", platform: "server" } },
          codegen: [
            {
              source: { command: ["emit-contract"], watch: ["contract.json"] },
              descriptor: "generated/contract.json",
              typescript: "generated/contract.ts",
              name: "Counter",
            },
          ],
        },
        root,
      );
      await generateBindings(config);
      const before = await stat(path.join(root, "generated/contract.ts"), { bigint: true });
      await writeFile(path.join(root, "facet.ts"), "export default 2;");
      await generateBindings(config);
      assert.equal(await readFile(path.join(root, "calls.txt"), "utf-8"), "1");
      const unchanged = await stat(path.join(root, "generated/contract.ts"), { bigint: true });
      assert.equal(unchanged.mtimeNs, before.mtimeNs);
      const changed = local("cache.changed", { version: 1, input: Type.Object({}), members: {} });
      await writeFile(path.join(root, "contract.json"), JSON.stringify(changed.descriptor));
      await generateBindings(config);
      assert.equal(await readFile(path.join(root, "calls.txt"), "utf-8"), "2");
      await writeFile(exporter, `${script}\n// changed exporter provenance\n`, { mode: 0o755 });
      await generateBindings(config);
      assert.equal(await readFile(path.join(root, "calls.txt"), "utf-8"), "3");
      process.env.PATH = `${path.join(root, "second")}${path.delimiter}${previousPath ?? ""}`;
      await generateBindings(config);
      assert.equal(await readFile(path.join(root, "calls.txt"), "utf-8"), "4");
      const [binding] = config.codegen;
      assert.ok(binding);
      const renamed = resolveConfig({
        ...config,
        codegen: [{ ...binding, name: "RenamedCounter" }],
      });
      await generateBindings(renamed);
      assert.equal(await readFile(path.join(root, "calls.txt"), "utf-8"), "5");
      const retained = await readFile(path.join(root, "generated/contract.ts"), "utf-8");
      await writeFile(path.join(root, "contract.json"), "invalid JSON");
      await assert.rejects(generateBindings(renamed), ToolingError);
      await assert.rejects(generateBindings(renamed), ToolingError);
      assert.equal(await readFile(path.join(root, "calls.txt"), "utf-8"), "7");
      assert.equal(await readFile(path.join(root, "generated/contract.ts"), "utf-8"), retained);
    } finally {
      if (previousPath === undefined) {
        delete process.env.PATH;
      } else {
        process.env.PATH = previousPath;
      }
      await rm(root, { recursive: true, force: true });
    }
  },
);

test("token snapshots invalidate transitive and compiler inputs while cache hits retain alias safety", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-token-cache-"));
  const countKey = `token:${root}`;
  try {
    await mkdir(path.join(root, "inputs"));
    await mkdir(path.join(root, "generated"));
    await symlink(path.join(root, "generated"), path.join(root, "alias"), "dir");
    const dependency = path.join(root, "inputs/out.ts");
    await writeFile(
      dependency,
      `export const descriptor = ${JSON.stringify(Contract.descriptor)};`,
    );
    const token = path.join(root, "token.ts");
    const source = `import { descriptor } from './inputs/out.js'; globalThis[${JSON.stringify(countKey)}] = (globalThis[${JSON.stringify(countKey)}] ?? 0) + 1; export const Counter = { descriptor };`;
    await writeFile(token, source);
    await writeFile(path.join(root, "tsconfig.json"), '{"extends":"./base.json"}');
    await writeFile(path.join(root, "base.json"), "{}");
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          { source: { module: "token.ts", export: "Counter" }, typescript: "alias/out.ts" },
        ],
      },
      root,
    );
    await generateBindings(config);
    await writeFile(path.join(root, "unrelated.ts"), "export default 1;");
    await generateBindings(config);
    assert.equal(Reflect.get(globalThis, countKey), 1);
    const changed = local("token.changed", { version: 1, input: Type.Object({}), members: {} });
    const authored = `export const descriptor = ${JSON.stringify(changed.descriptor)};`;
    await writeFile(dependency, authored);
    await generateBindings(config);
    assert.equal(Reflect.get(globalThis, countKey), 2);
    await writeFile(path.join(root, "base.json"), '{"compilerOptions":{"strict":true}}');
    await generateBindings(config);
    assert.equal(Reflect.get(globalThis, countKey), 3);
    await writeFile(token, `${source}\n// source changed\n`);
    await generateBindings(config);
    assert.equal(Reflect.get(globalThis, countKey), 4);
    await rm(path.join(root, "alias"));
    await symlink(path.join(root, "inputs"), path.join(root, "alias"), "dir");
    await assert.rejects(
      generateBindings(config),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "config",
    );
    assert.equal(Reflect.get(globalThis, countKey), 4);
    assert.equal(await readFile(dependency, "utf-8"), authored);
  } finally {
    Reflect.deleteProperty(globalThis, countKey);
    await rm(root, { recursive: true, force: true });
  }
});

test("changing one binding leaves completed sibling token snapshots reusable and repairs missing outputs", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-sibling-cache-"));
  const firstKey = `first:${root}`;
  const secondKey = `second:${root}`;
  try {
    await writeFile(path.join(root, "first.ts"), countedToken(firstKey));
    await writeFile(path.join(root, "second.ts"), countedToken(secondKey));
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          { source: { module: "first.ts", export: "Counter" }, typescript: "generated/first.ts" },
          { source: { module: "second.ts", export: "Counter" }, typescript: "generated/second.ts" },
        ],
      },
      root,
    );
    await generateBindings(config);
    await writeFile(path.join(root, "first.ts"), `${countedToken(firstKey)}\n// changed\n`);
    await generateBindings(config);
    assert.equal(Reflect.get(globalThis, firstKey), 2);
    assert.equal(Reflect.get(globalThis, secondKey), 1);
    await rm(path.join(root, "generated/second.ts"));
    await generateBindings(config);
    assert.equal(Reflect.get(globalThis, secondKey), 1);
    assert.match(
      await readFile(path.join(root, "generated/second.ts"), "utf-8"),
      /fromDescriptor/u,
    );
  } finally {
    Reflect.deleteProperty(globalThis, firstKey);
    Reflect.deleteProperty(globalThis, secondKey);
    await rm(root, { recursive: true, force: true });
  }
});

test("failed generations and cancellation after module effects never adopt descriptor snapshots", async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), "tembra-cache-refusal-"));
  const countKey = `counter:${root}`;
  const hookKey = `abort:${root}`;
  try {
    await writeFile(path.join(root, "token.ts"), countedToken(countKey, hookKey));
    await writeFile(path.join(root, "invalid.json"), "invalid JSON");
    const config = resolveConfig(
      {
        builds: {},
        codegen: [
          { source: { module: "token.ts", export: "Counter" }, typescript: "generated/token.ts" },
          { source: { descriptor: "invalid.json" }, typescript: "generated/invalid.ts" },
        ],
      },
      root,
    );
    await assert.rejects(generateBindings(config), ToolingError);
    await assert.rejects(generateBindings(config), ToolingError);
    assert.equal(Reflect.get(globalThis, countKey), 2);
    const [binding] = config.codegen;
    assert.ok(binding);
    const valid = resolveConfig({ ...config, codegen: [binding] });
    const controller = new AbortController();
    Reflect.set(globalThis, hookKey, () => {
      controller.abort(new Error("after module effects"));
    });
    await assert.rejects(
      generateBindings(valid, { signal: controller.signal }),
      (failure: unknown) => failure instanceof ToolingError && failure.phase === "cancelled",
    );
    assert.equal(Reflect.get(globalThis, countKey), 3);
    Reflect.deleteProperty(globalThis, hookKey);
    await generateBindings(valid);
    await generateBindings(valid);
    assert.equal(Reflect.get(globalThis, countKey), 4);
  } finally {
    Reflect.deleteProperty(globalThis, countKey);
    Reflect.deleteProperty(globalThis, hookKey);
    await rm(root, { recursive: true, force: true });
  }
});
