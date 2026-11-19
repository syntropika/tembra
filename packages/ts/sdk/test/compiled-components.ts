// Compiled Orbs in prepared browser components, on a real host and a real contract, for the tests
// of the kernel's interfaces (`tembra:guest`). The component is written in JavaScript: its exports
// report export tasks as the instrumented glue does, and it calls the import objects the browser
// adapter gives a generated binding. Real JCO glue and Chrome execution are qualified by the
// browser acceptance.
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import path from "node:path";
import { isolate, type AdaptedOrb } from "#tembra-adapters";
import { createHost } from "../src/node.ts";
import type { Host } from "../src/host.ts";
import { ComponentPort } from "../src/interpreter/component/port.ts";
import {
  compiledDescription,
  ComponentRevision,
  type CompiledRoles,
  type Described,
} from "../src/interpreter/component/revision.ts";
import { query, type ContractPort, type InstallationPlan } from "../src/interpreter/types.ts";

const engine = createRequire(import.meta.url)("@syntropika/tembra-engine-node") as {
  NodeContract: new (
    directory: string,
    world: string,
  ) => {
    sourceArtifact(source: string, name: string, apiVersion: number): Uint8Array;
  };
  NodeArtifact: new (bytes: Uint8Array) => ContractPort;
};

/** The checked contract of `world` in `wit`; the contract model is the same for any artifact. */
export async function contract(wit: string, world: string): Promise<ContractPort> {
  const directory = await mkdtemp(path.join(tmpdir(), "tembra-kernel-"));
  try {
    await writeFile(path.join(directory, "orb.wit"), wit);
    const bytes = new engine.NodeContract(directory, world).sourceArtifact("", world, 1);
    return new engine.NodeArtifact(bytes);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

type Exports = Record<string, Record<string, (...args: any[]) => unknown>>;
/** The import objects by binding, as the generated binding calls them. */
type Imports = Record<string, any>;

/** Report each export call as one export task with its real exit, as the instrumented glue does. */
function instrument(port: ComponentPort, exports: Exports): Exports {
  let current: number | null = null;
  let next = 0;
  port.observer.attach({ currentRootTask: () => current });
  const task =
    (entry: string, body: (...args: any[]) => unknown) =>
    (...args: unknown[]) => {
      const id = ++next;
      port.observer.taskStarted({ id, entry, isAsync: true });
      const exit = () => port.observer.taskExited({ id });
      current = id;
      let result: unknown;
      try {
        result = body(...args);
      } catch (error) {
        exit();
        throw error;
      } finally {
        current = null;
      }
      return Promise.resolve(result).finally(exit);
    };
  return Object.fromEntries(
    Object.entries(exports).map(([name, functions]) => [
      name,
      Object.fromEntries(
        Object.entries(functions).map(([fn, body]) => [fn, task(`${name}#${fn}`, body)]),
      ),
    ]),
  );
}

/** One compiled Orb: each installation and reload gets a fresh revision of `component`. */
export interface Compiled {
  readonly name: string;
  readonly contract: ContractPort;
  readonly roles?: Partial<CompiledRoles>;
  component(imports: Imports, revision: number): Exports;
}

/** A host whose isolated backend opens the test's components for `{ artifact: [index] }`. */
export function host(components: readonly Compiled[], logs: unknown[] = []): Host {
  const revisions = new Map<string, number>();
  return createHost({
    principal: "host",
    ...{
      [isolate]: async (definition: { artifact: Uint8Array; requires?: object }) => {
        const compiled = components[definition.artifact[0]!]!;
        const { name, contract } = compiled;
        const revision = (revisions.get(name) ?? 0) + 1;
        revisions.set(name, revision);
        const port = new ComponentPort(2000, async () => {});
        const graph = query<Described>(contract, { kind: "describe" });
        const adapter = new ComponentRevision(port, contract, graph, 2000, {
          orb: name,
          observer: (record) => logs.push(record),
        });
        // The import objects a generated binding receives, built before instantiation.
        const imports: Imports = Object.fromEntries(
          graph.imports.map((entry) => [entry.binding, adapter.kernel.imports(entry.binding)]),
        );
        port.exports = instrument(port, compiled.component(imports, revision)) as never;
        const identity = { name, apiVersion: 1 };
        const plan = query<InstallationPlan>(contract, {
          kind: "install",
          ...identity,
          requires: definition.requires ?? {},
          grants: {},
        });
        return adapter.definition(
          identity,
          plan,
          compiledDescription(graph, identity, { contributions: {}, ...compiled.roles }),
        ) as AdaptedOrb;
      },
    },
  });
}
