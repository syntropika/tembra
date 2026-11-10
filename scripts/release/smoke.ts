// Install the packed npm archives into an empty project and execute them as a consumer would.
// Usage: bun scripts/release/smoke.ts <npm-artifacts> <empty-project> --provider <component.wasm>
import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: { provider: { type: "string" }, "browser-orb": { type: "string" } },
});
const [archives, project] = positionals;
if (!archives || !project || !values.provider)
  throw new Error(
    "Usage: bun scripts/release/smoke.ts <npm-artifacts> <empty-project> --provider <component.wasm>",
  );
const directory = path.resolve(project);
const provider = path.resolve(values.provider);
const workspace: {
  dependencies: Record<string, string>;
  devDependencies: Record<string, string>;
} = JSON.parse(readFileSync(new URL("../../package.json", import.meta.url), "utf8"));
const packages: { name: string; filename: string }[] = JSON.parse(
  readFileSync(path.join(archives, "packages.json"), "utf8"),
);

mkdirSync(path.join(directory, "orb"), { recursive: true });
// Remove only outputs that this script generates inside the consumer project.
for (const output of ["artifacts", "build"])
  rmSync(path.join(directory, output), { recursive: true, force: true });
writeFileSync(
  path.join(directory, "package.json"),
  JSON.stringify({ private: true, type: "module" }),
);
// The consumer compiler and Node types are explicit, pinned to the workspace versions.
execFileSync(
  "npm",
  [
    "install",
    "--ignore-scripts",
    "--omit=optional",
    "--no-audit",
    "--no-fund",
    "--no-package-lock",
    ...packages
      .filter((item) => item.name !== "@syntropika/tembra-react-native")
      .map((item) => path.resolve(archives, item.filename)),
    `typescript@${workspace.dependencies.typescript}`,
    `@types/node@${workspace.devDependencies["@types/node"]}`,
  ],
  { cwd: directory, stdio: "inherit" },
);

// A minimal source Orb calls the compiled provider through the same WIT import.
writeFileSync(
  path.join(directory, "orb/contract.wit"),
  `package example:integrated@1.0.0;

interface arithmetic {
    double: func(value: s32) -> s32;
}

interface relay {
    quadruple: async func(value: s32) -> s32;
}

world arithmetic-provider { export arithmetic; }
world relay-provider {
    import arithmetic;
    export relay;
}
`,
);
writeFileSync(
  path.join(directory, "orb/relay.ts"),
  `import { orb } from "@syntropika/tembra-sdk";
import type { Context, Declaration, WitMethods } from "@syntropika/tembra-sdk";
import type { relay, ExampleIntegratedArithmetic100 as Arithmetic } from "./contract.wit";

function create(self: Declaration) {
  return {
    arithmetic: self.get<typeof Arithmetic>("example:integrated/arithmetic@1.0.0", ["main"]),
  };
}

type Environment = ReturnType<typeof create>;

export default orb({
  name: "smoke",
  apiVersion: 1,
  services: {
    relay: {
      create,
      methods: {
        async quadruple(environment: Environment, context: Context, value: number) {
          const doubled: number = await environment.arithmetic.double(value, context);
          return environment.arithmetic.double(doubled, context);
        },
      } satisfies WitMethods<typeof relay, Environment>,
    },
  },
});
`,
);
writeFileSync(
  path.join(directory, "orb/tsconfig.json"),
  JSON.stringify({
    compilerOptions: {
      target: "ES2023",
      module: "ESNext",
      moduleResolution: "Bundler",
      strict: true,
      noEmit: true,
      allowArbitraryExtensions: true,
      customConditions: ["tembra-wasm"],
      types: [],
    },
    include: ["relay.ts", "contract.d.wit.ts"],
  }),
);
writeFileSync(
  path.join(directory, "orb/build.ts"),
  `import { writeFileSync } from "node:fs";
import path from "node:path";
import { build } from "@syntropika/tembra-sdk/tooling";

const [provider, out] = process.argv.slice(2);
const artifacts = await build({
  root: import.meta.dir,
  outDir: out,
  builds: {
    arithmetic: { entry: provider!, wit: "contract.wit", world: "arithmetic-provider", apiVersion: 1 },
    relay: { entry: "relay.ts", wit: "contract.wit", world: "relay-provider" },
  },
});
writeFileSync(
  path.join(out!, "artifacts.json"),
  JSON.stringify(Object.fromEntries(artifacts.map((artifact) => [artifact.name, artifact.path]))),
);
`,
);
const artifacts = path.join(directory, "artifacts");
// Tembra build tooling runs on Bun; it uses only the installed SDK and engine packages.
execFileSync(process.execPath, [path.join(directory, "orb/build.ts"), provider, artifacts], {
  cwd: directory,
  stdio: "inherit",
});
const built: { relay: string; arithmetic: string } = JSON.parse(
  readFileSync(path.join(artifacts, "artifacts.json"), "utf8"),
);

// Every published SDK entry must resolve to generated declarations under ordinary NodeNext rules.
const sdk: { exports: Record<string, unknown> } = JSON.parse(
  readFileSync(path.join(directory, "node_modules/@syntropika/tembra-sdk/package.json"), "utf8"),
);
const entries = Object.keys(sdk.exports)
  .map(
    (entry, index) =>
      `import type * as Entry${index} from "${path.posix.join("@syntropika/tembra-sdk", entry)}";`,
  )
  .join("\n");
writeFileSync(
  path.join(directory, "consumer.ts"),
  `import { readFileSync } from "node:fs";
${entries}
import { TembraError } from "@syntropika/tembra-sdk";
import { createHost as nodeHost } from "@syntropika/tembra-sdk/node";
import init from "@syntropika/tembra-engine-wasm";
import { interpreterURL } from "@syntropika/tembra-engine-wasm/assets";

const [relayFile, arithmeticFile] = process.argv.slice(2);
if (!relayFile || !arithmeticFile) throw new Error("Usage: consumer <relay.wasm> <arithmetic.wasm>");

const interpreter = interpreterURL();
if (interpreter.href !== import.meta.resolve("@syntropika/tembra-engine-wasm/interpreter"))
  throw new Error("Packed interpreter locator differs from the exported asset");
await WebAssembly.compile(new Uint8Array(readFileSync(interpreter)));
await init({
  module_or_path: new Uint8Array(
    readFileSync(new URL(import.meta.resolve("@syntropika/tembra-engine-wasm/tembra_bg.wasm"))),
  ),
});
const { createHost: browserHost } = await import("@syntropika/tembra-sdk/browser");

for (const [label, create] of [
  ["Node", nodeHost],
  ["WASM", browserHost],
] as const) {
  const host = await create();
  try {
    // Errors from every entry must be instances of the class exported by the package root.
    const failure = await host.get("missing-service", "main").then(
      () => undefined,
      (error: unknown) => error,
    );
    if (!(failure instanceof TembraError))
      throw new Error(\`\${label} host error is not the exported TembraError: \${String(failure)}\`);
    await host.install({
      name: "packed",
      apiVersion: 1,
      services: { packed: { methods: { read: () => 42 } } },
    });
    const reference = await host.get("packed", "main");
    if ((await reference.read()) !== 42) throw new Error(\`\${label} trusted runtime failed\`);
    reference.release();
  } finally {
    await host.close();
  }
}

// Execute the isolated source Orb and the compiled provider through the installed interpreter.
const host = nodeHost();
try {
  await host.load(arithmeticFile);
  await host.load(relayFile, {
    requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
  });
  const arithmetic = await host.get("arithmetic::example:integrated/arithmetic@1.0.0", "main");
  if ((await arithmetic.double(21)) !== 42) throw new Error("Compiled provider failed");
  const relay = await host.get("smoke::example:integrated/relay@1.0.0", "main");
  if ((await relay.quadruple(5)) !== 20) throw new Error("Isolated source Orb failed");
  relay.release();
  arithmetic.release();
} finally {
  await host.close();
}
console.log("Packed trusted, isolated source and compiled Orbs passed");
`,
);
writeFileSync(
  path.join(directory, "tsconfig.json"),
  JSON.stringify({
    compilerOptions: {
      target: "ES2023",
      module: "NodeNext",
      moduleResolution: "NodeNext",
      strict: true,
      skipLibCheck: false,
      verbatimModuleSyntax: true,
      types: ["node"],
      rootDir: ".",
      outDir: "build",
    },
    files: ["consumer.ts"],
  }),
);
execFileSync("node", [path.join(directory, "node_modules/typescript/bin/tsc"), "-p", directory], {
  cwd: directory,
  stdio: "inherit",
});
// Browser consumers need only the platform libraries, without Node or virtual WASI modules.
writeFileSync(
  path.join(directory, "browser-types.ts"),
  `
import type * as Browser from "@syntropika/tembra-sdk/browser";
import type * as Http from "@syntropika/tembra-sdk/http";
export type Capabilities = [Browser.HttpProvider, Browser.HttpProviderError, Http.HttpRequest, Http.HttpResponse, Http.HttpError];
`,
);
writeFileSync(
  path.join(directory, "browser-tsconfig.json"),
  JSON.stringify({
    compilerOptions: {
      target: "ES2023",
      module: "ESNext",
      moduleResolution: "Bundler",
      lib: ["ES2023", "DOM", "DOM.Iterable"],
      strict: true,
      skipLibCheck: false,
      noEmit: true,
      allowImportingTsExtensions: true,
      types: [],
    },
    files: ["browser-types.ts"],
  }),
);
execFileSync(
  "node",
  [
    path.join(directory, "node_modules/typescript/bin/tsc"),
    "-p",
    path.join(directory, "browser-tsconfig.json"),
  ],
  {
    cwd: directory,
    stdio: "inherit",
  },
);
// Node runs the compiled JavaScript; Bun keeps its supported source condition.
const inputs = [built.relay, built.arithmetic];
execFileSync("node", [path.join(directory, "build/consumer.js"), ...inputs], {
  cwd: directory,
  stdio: "inherit",
});
execFileSync(process.execPath, [path.join(directory, "consumer.ts"), ...inputs], {
  cwd: directory,
  stdio: "inherit",
});
if (values["browser-orb"])
  execFileSync(
    process.execPath,
    [
      path.resolve(import.meta.dir, "../../packages/ts/test/packed-browser.ts"),
      directory,
      path.resolve(values["browser-orb"]),
    ],
    {
      stdio: "inherit",
    },
  );
