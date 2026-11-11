/**
 * Every authored source fixture builds as a real component artifact: the production typecheck
 * against its world's WIT types, the closed ES module bundle and `NodeContract.sourceArtifact`
 * with the packaged QuickJS runtime, which verifies the component against the public contract.
 */
import { afterAll, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { NodeArtifact } from "@syntropika/tembra-engine-node";
import { build } from "../src/tooling/index.ts";

const sdk = path.resolve(import.meta.dir, "..");
const fixtures = path.join(sdk, "test/fixtures");
const hooksWit = path.resolve(sdk, "../../../crates/executor-wasm/tests/wit/hooks/hooks.wit");
const guest = JSON.stringify(path.join(sdk, "src/guest.ts"));
const http = JSON.stringify(path.join(sdk, "src/http.ts"));
const root = await mkdtemp(
  path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-sources-"),
);
afterAll(() => rm(root, { recursive: true, force: true }));

/** [fixture file, world, expected Orb name, WIT file when not the fixture's contract.wit] */
const SOURCES: readonly (readonly [string, string, string, string?])[] = [
  ["wasm-orb/orb.ts", "calculator", "calculator"],
  ["wasm-orb/online.ts", "online-calculator", "online"],
  ["wasm-keyed/provider.ts", "provider", "keyed"],
  ["wasm-keyed/router.ts", "consumer", "router"],
  ["wasm-hooks/provider.ts", "provider", "tools", hooksWit],
  ["wasm-hooks/extension.ts", "extension", "extension", hooksWit],
  ["wasm-filesystem/orb.ts", "filesystem", "filesystem"],
  ["wasm-http/orb.ts", "client", "http"],
  ["wasm-sockets/orb.ts", "sockets", "sockets"],
  ["wasm-system/orb.ts", "system", "system"],
  ["wasm-system/ambient.ts", "ambient", "ambient"],
  ["wasm-standard/orb.ts", "standard", "standard"],
  ["wasm-standard/boundary.ts", "boundary", "boundary"],
  ["wasm-precision/provider.ts", "provider", "precision"],
  ["wasm-precision/consumer.ts", "consumer", "relay"],
  ["wasm-stream-source/orb.ts", "producer", "typed-stream"],
  ["wasm-stream-source/consumer.ts", "consumer", "typed-stream-consumer"],
  ["wasm-ingress/orb.ts", "handler", "ingress"],
  ["wasm-ingress/ledger.ts", "provider", "ledger"],
  ["wasm-ingress/carrier.ts", "handler-only", "carrier"],
];

/** Copy one fixture beside its WIT, with SDK imports resolved to this checkout. */
async function prepare(file: string, world: string, wit?: string): Promise<string> {
  const directory = path.join(root, `${path.dirname(file)}-${world}`);
  await mkdir(directory, { recursive: true });
  await writeFile(
    path.join(directory, "contract.wit"),
    await readFile(wit ?? path.join(fixtures, path.dirname(file), "contract.wit")),
  );
  const source = (await readFile(path.join(fixtures, file), "utf8"))
    .replaceAll('"@syntropika/tembra-sdk/http"', http)
    .replaceAll('"@syntropika/tembra-sdk"', guest);
  await writeFile(path.join(directory, path.basename(file)), source);
  return directory;
}

describe("authored source fixtures build as verified components", () => {
  for (const [file, world, name, wit] of SOURCES)
    test(`${file} (${world})`, { timeout: 120_000 }, async () => {
      const directory = await prepare(file, world, wit);
      const [artifact] = await build({
        root: directory,
        outDir: path.join(directory, "dist"),
        builds: {
          orb: { entry: path.basename(file), wit: "contract.wit", world, minify: false },
        },
      });
      const description = JSON.parse(
        new NodeArtifact(new Uint8Array(await readFile(artifact!.path))).description(),
      );
      expect(JSON.stringify(description)).toContain(JSON.stringify(name));
    });
});

describe("source build refusals", () => {
  test("console output during module evaluation fails the build", async () => {
    const directory = await prepare("wasm-keyed/provider.ts", "provider-console");
    await writeFile(
      path.join(directory, "contract.wit"),
      await readFile(path.join(fixtures, "wasm-keyed/contract.wit")),
    );
    await writeFile(
      path.join(directory, "orb.ts"),
      `import { orb } from ${guest};
console.log("module log");
export default orb({ name: "keyed", apiVersion: 1, services: { counter: { methods: { add: (_e: unknown, _c: unknown, amount: number) => amount, who: () => "" } } } });`,
    );
    await expect(
      build({
        root: directory,
        outDir: path.join(directory, "dist"),
        builds: { orb: { entry: "orb.ts", wit: "contract.wit", world: "provider", minify: false } },
      }),
    ).rejects.toThrow(/module evaluation has no host/);
  });

  test("a Promise implementation of a synchronous WIT function fails typechecking", async () => {
    const directory = await prepare("wasm-keyed/provider.ts", "provider-async");
    await writeFile(
      path.join(directory, "orb.ts"),
      `import { orb, type WitMethods } from ${guest};
import type { counter } from "./contract.wit";
export default orb({ name: "keyed", apiVersion: 1, services: { counter: { methods: {
  async add(_e: unknown, _c: unknown, amount: number) { return amount; },
  who: () => "",
} satisfies WitMethods<typeof counter, unknown> } } });`,
    );
    await expect(
      build({
        root: directory,
        outDir: path.join(directory, "dist"),
        builds: { orb: { entry: "orb.ts", wit: "contract.wit", world: "provider", minify: false } },
      }),
    ).rejects.toThrow(/Promise<number>/);
  });
});
