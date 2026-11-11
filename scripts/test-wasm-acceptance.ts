// Execute real source and compiled Orbs on native Rust, Node and Chrome from one set of artifacts.
// The packaged interpreter must already exist; see DEVELOP.md#isolated-orbs.
import { spawnSync } from "node:child_process";
import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import path from "node:path";
import { parseArgs } from "node:util";
import { stagePackage } from "./release/npm.ts";

const suites = ["native", "sdk", "node", "browser", "example", "package"];
const usage =
  "Usage: bun scripts/test-wasm-acceptance.ts --out <directory> --provider <component.wasm> " +
  `--probe <component.wasm> --consumer <component.wasm> --contributor <component.wasm> --http-client <component.wasm> --sockets <component.wasm> --http-handler <component.wasm> --http-proxy <component.wasm> --resource-provider <component.wasm> --resource-consumer <component.wasm> --stream-producer <component.wasm> --stream-consumer <component.wasm> [--fixtures <directory>] [--suite <${suites.join("|")}>]...`;
const { values } = parseArgs({
  options: {
    out: { type: "string" },
    provider: { type: "string" },
    probe: { type: "string" },
    consumer: { type: "string" },
    contributor: { type: "string" },
    "http-client": { type: "string" },
    sockets: { type: "string" },
    "http-handler": { type: "string" },
    "http-proxy": { type: "string" },
    "resource-provider": { type: "string" },
    "resource-consumer": { type: "string" },
    "stream-producer": { type: "string" },
    "stream-consumer": { type: "string" },
    fixtures: { type: "string" },
    suite: { type: "string", multiple: true },
  },
});
if (
  !values.out ||
  !values.provider ||
  !values.probe ||
  !values.consumer ||
  !values.contributor ||
  !values["http-client"] ||
  !values.sockets ||
  !values["http-handler"] ||
  !values["http-proxy"] ||
  !values["resource-provider"] ||
  !values["resource-consumer"] ||
  !values["stream-producer"] ||
  !values["stream-consumer"]
)
  throw new Error(usage);
const selected = values.suite ?? suites;
for (const name of selected) if (!suites.includes(name)) throw new Error(usage);

const root = path.resolve(import.meta.dir, "..");
const out = path.resolve(values.out);
const relative = path.relative(out, root);
// The runner replaces its own subdirectories, so the output cannot contain the checkout.
if (relative === "" || (!relative.startsWith("..") && !path.isAbsolute(relative)))
  throw new Error("Choose an output directory that does not contain the checkout");
const provider = path.resolve(values.provider);
const probe = path.resolve(values.probe);
const consumer = path.resolve(values.consumer);
const contributor = path.resolve(values.contributor);
const httpClient = path.resolve(values["http-client"]);
const sockets = path.resolve(values.sockets);
const httpHandler = path.resolve(values["http-handler"]);
const httpProxy = path.resolve(values["http-proxy"]);
const resourceProvider = path.resolve(values["resource-provider"]);
const resourceConsumer = path.resolve(values["resource-consumer"]);
const streamProducer = path.resolve(values["stream-producer"]);
const streamConsumer = path.resolve(values["stream-consumer"]);
const engine = path.join(root, "packages/ts/engine-node/dist/interpreter.wasm");
const browserEngine = path.join(root, "packages/ts/engine-wasm/dist/interpreter");
for (const file of [
  engine,
  path.join(browserEngine, "interpreter.wasm"),
  provider,
  probe,
  consumer,
  contributor,
  httpClient,
  sockets,
  httpHandler,
  httpProxy,
  resourceProvider,
  resourceConsumer,
  streamProducer,
  streamConsumer,
])
  if (!existsSync(file)) throw new Error(`Missing acceptance input: ${file}`);
// Native, Node and browser suites must qualify the same packaged interpreter bytes.
if (!readFileSync(engine).equals(readFileSync(path.join(browserEngine, "interpreter.wasm"))))
  throw new Error("The Node and browser packages contain different interpreters");
const temporary = path.join(out, "tmp");
mkdirSync(temporary, { recursive: true });

/** Run one step with inherited output and stop on the first failure. */
function run(
  command: string,
  args: string[],
  env: Record<string, string> = {},
  capture = false,
): string {
  console.log(`\n$ ${[command, ...args].join(" ")}`);
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: capture ? ["ignore", "pipe", "inherit"] : "inherit",
    encoding: "utf8",
    env: { ...process.env, TEMBRA_TEST_ARTIFACTS: temporary, ...env },
  });
  if (result.error) throw result.error;
  if (result.status !== 0)
    throw new Error(`${command} ${args[0]} failed with ${result.status ?? result.signal}`);
  return result.stdout ?? "";
}

/** Replace one runner-owned subdirectory of the output. */
function fresh(name: string): string {
  const directory = path.join(out, name);
  rmSync(directory, { recursive: true, force: true });
  mkdirSync(directory, { recursive: true });
  return directory;
}

/** Return the only artifact that the SDK build wrote for one target. */
function artifact(directory: string): string {
  const files = readdirSync(directory).filter((file) => file.endsWith(".wasm"));
  if (files.length !== 1) throw new Error(`Expected one artifact in ${directory}`);
  return path.join(directory, files[0]!);
}

const bun = process.execPath;
const fixtures = values.fixtures ? path.resolve(values.fixtures) : path.join(out, "fixtures");
if (!values.fixtures)
  run(bun, [
    "scripts/test-wasm-fixtures.ts",
    "--out",
    fixtures,
    "--provider",
    provider,
    "--contributor",
    contributor,
    "--consumer",
    consumer,
    "--probe",
    probe,
    "--http-client",
    httpClient,
    "--http-handler",
    httpHandler,
    "--resource-provider",
    resourceProvider,
    "--stream-producer",
    streamProducer,
  ]);
const environment = {
  TEMBRA_WASM_ENGINE: engine,
  TEMBRA_WASM_FIXTURES: fixtures,
  TEMBRA_WASM_ORB: path.join(fixtures, "calculator.wasm"),
  TEMBRA_WASM_PROVIDER: provider,
  TEMBRA_WASM_PROBE: probe,
  TEMBRA_WASM_CONSUMER: consumer,
  TEMBRA_WASM_CONTRIBUTOR: contributor,
  TEMBRA_WASM_HTTP_CLIENT: httpClient,
  TEMBRA_WASM_SOCKETS: sockets,
  TEMBRA_WASM_HTTP_HANDLER: httpHandler,
  TEMBRA_WASM_HTTP_PROXY: httpProxy,
  TEMBRA_WASM_RESOURCE_PROVIDER: resourceProvider,
  TEMBRA_WASM_RESOURCE_CONSUMER: resourceConsumer,
  TEMBRA_WASM_STREAM_PRODUCER: streamProducer,
  TEMBRA_WASM_STREAM_CONSUMER: streamConsumer,
  TEMBRA_WASM_SOURCE_HANDLER: path.join(fixtures, "ingress-carrier.wasm"),
};
const timings: string[] = [];
for (const suite of selected) {
  const started = performance.now();
  if (suite === "native") {
    // Acceptance runs both self-contained components and tests requiring these real artifacts.
    run(
      "cargo",
      ["test", "--locked", "-p", "tembra-executor-wasm", "--tests", "--", "--include-ignored"],
      {
        ...environment,
        TEMBRA_WASM_SCRATCH: fresh("scratch"),
      },
    );
  } else if (suite === "sdk") {
    run(bun, ["run", "--cwd", "packages/ts/sdk", "test"], environment);
  } else if (suite === "node") {
    run(bun, ["run", "--cwd", "packages/ts/sdk", "test:node"], {
      ...environment,
      UV_THREADPOOL_SIZE: "1",
    });
  } else if (suite === "browser") {
    // The packaged default runs the full suite, then a copied override directory runs it again.
    run(bun, ["scripts/test-wasm-browser.ts"], environment);
    const alternate = fresh("browser-engine");
    cpSync(browserEngine, alternate, { recursive: true });
    run(bun, ["scripts/test-wasm-browser.ts"], {
      ...environment,
      TEMBRA_WASM_BROWSER_ENGINE: alternate,
    });
    run(bun, ["run", "test:browser"]);
  } else if (suite === "example") {
    const example = fresh("example");
    run(bun, ["examples/wasm-orb/build.ts", "--provider", provider, "--out", example]);
    const consumer = spawnSync(
      bun,
      [
        "examples/wasm-orb/consumer.ts",
        artifact(path.join(example, "calculator")),
        artifact(path.join(example, "arithmetic")),
      ],
      { cwd: root, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"] },
    );
    if (consumer.error) throw consumer.error;
    process.stdout.write(consumer.stdout);
    const lines = consumer.stdout.trim().split("\n");
    if (
      consumer.status !== 0 ||
      lines[0] !== "24" ||
      !/resource: 42\b/.test(consumer.stdout) ||
      !/fetched: 120\b/.test(consumer.stdout)
    )
      throw new Error("The Wasm Orb example did not print its documented results");
  } else {
    const output = fresh("packed");
    const archives = path.join(output, "npm");
    mkdirSync(archives);
    const version = JSON.parse(readFileSync(path.join(root, "package.json"), "utf8")).version;
    const packages = [];
    for (const name of ["engine-node", "engine-wasm", "sdk"]) {
      const staged = stagePackage(root, name, output, version);
      const [packed] = JSON.parse(
        run(
          "npm",
          ["pack", staged, "--ignore-scripts", "--json", "--pack-destination", archives],
          {},
          true,
        ),
      );
      packages.push({ name: packed.name, filename: packed.filename });
    }
    writeFileSync(path.join(archives, "packages.json"), JSON.stringify(packages));
    run(bun, [
      "scripts/release/smoke.ts",
      archives,
      path.join(output, "consumer"),
      "--provider",
      provider,
      "--browser-orb",
      path.join(fixtures, "arithmetic-source.wasm"),
    ]);
  }
  timings.push(`${suite}: ${((performance.now() - started) / 1000).toFixed(1)}s`);
}
console.log(`\nIsolated Orb acceptance passed (${timings.join(", ")})`);
