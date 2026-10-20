import { existsSync, readFileSync } from "node:fs";
import { readdir, readFile } from "node:fs/promises";
import path from "node:path";

const root = import.meta.dirname;
const assets: Record<string, string> = {};
const collected = new Set<string>();

function manifest(value: unknown): { name: string; dependencies: Record<string, string> } {
  if (
    value === null ||
    typeof value !== "object" ||
    !("name" in value) ||
    typeof value.name !== "string"
  ) {
    throw new TypeError("Package asset manifest must declare a name");
  }
  const dependencies: Record<string, string> = {};
  if (
    "dependencies" in value &&
    value.dependencies !== null &&
    typeof value.dependencies === "object"
  ) {
    for (const [name, version] of Object.entries(value.dependencies)) {
      if (typeof version !== "string") {
        throw new TypeError(`Invalid dependency asset declaration: ${name}`);
      }
      dependencies[name] = version;
    }
  }
  return { name: value.name, dependencies };
}
function packageRoot(name: string, from: string): string {
  let entry: string;
  try {
    entry = Bun.resolveSync(`${name}/package.json`, from);
  } catch {
    entry = Bun.resolveSync(name, from);
  }
  let directory = path.dirname(entry);
  for (;;) {
    const candidate = path.join(directory, "package.json");
    if (
      existsSync(candidate) &&
      manifest(JSON.parse(readFileSync(candidate, "utf-8"))).name === name
    ) {
      return directory;
    }
    const parent = path.dirname(directory);
    if (parent === directory) {
      throw new Error(`Cannot locate embedded package ${name}`);
    }
    directory = parent;
  }
}
async function joined(tasks: Promise<void>[]): Promise<void> {
  const results = await Promise.allSettled(tasks);
  for (const result of results) {
    if (result.status === "rejected") {
      throw result.reason;
    }
  }
}
async function collectFiles(directory: string, prefix: string): Promise<void> {
  const entries = await readdir(directory, { withFileTypes: true });
  await joined(
    entries.map(async (entry) => {
      if (entry.name === "node_modules" || entry.name === ".git") {
        return;
      }
      const source = path.join(directory, entry.name);
      const destination = `${prefix}/${entry.name}`;
      if (entry.isDirectory()) {
        await collectFiles(source, destination);
      } else if (/\.(?:[cm]?js|[cm]?ts|json)$/u.test(entry.name)) {
        assets[destination] = await readFile(source, "utf-8");
      }
    }),
  );
}

async function collectPackage(name: string, from: string): Promise<void> {
  if (collected.has(name)) {
    return;
  }
  collected.add(name);
  const directory = packageRoot(name, from);
  if (name === "@tembra/sdk") {
    assets[`node_modules/${name}/package.json`] = await readFile(
      path.join(directory, "package.json"),
      "utf-8",
    );
    await collectFiles(path.join(directory, "dist/src"), `node_modules/${name}/dist/src`);
  } else {
    await collectFiles(directory, `node_modules/${name}`);
  }
  const info = manifest(JSON.parse(await readFile(path.join(directory, "package.json"), "utf-8")));
  await joined(
    Object.keys(info.dependencies).map(async (dependency) => {
      await collectPackage(dependency, directory);
    }),
  );
}

await collectPackage("@tembra/sdk", root);
await collectPackage("@types/bun", root);
const compiler = path.dirname(Bun.resolveSync("typescript", root));
const libraries = await readdir(compiler);
await joined(
  libraries.map(async (file) => {
    if (/^lib.*\.d\.ts$/u.test(file)) {
      assets[`node_modules/typescript/lib/${file}`] = await readFile(
        path.join(compiler, file),
        "utf-8",
      );
    }
  }),
);
const result = await Bun.build({
  entrypoints: [path.join(root, "bin/tembra.ts")],
  target: "bun",
  compile: {
    outfile: path.join(root, "dist/bin/tembra"),
    autoloadDotenv: false,
    autoloadBunfig: false,
  },
  plugins: [
    {
      name: "tembra-embedded-assets",
      setup(builder) {
        builder.onLoad({ filter: /[/\\]embedded-assets\.[jt]s$/u }, () => ({
          contents: `export const embeddedAssets = ${JSON.stringify(assets)};`,
          loader: "ts",
        }));
      },
    },
  ],
});
if (!result.success) {
  throw new AggregateError(result.logs, "Cannot compile the Tembra executable");
}
