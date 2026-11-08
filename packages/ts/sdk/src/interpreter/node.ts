import { componentDefinition } from "./component.ts";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import type { NodeContract, NodeInterpreter } from "@syntropika/tembra-engine-node";
import type { OrbDefinition } from "../authoring/types.ts";
import { materialize, type AdaptedOrb } from "@syntropika/tembra-sdk/internal/adapters";
import { Revision } from "./revision.ts";
import type { InterpreterOrbOptions } from "./types.ts";
export type { Grants, InterpreterOrbOptions, PreparedSource, ProviderBinding } from "./types.ts";

export interface DirectoryFilesystem {
  readonly kind: "directory";
  readonly path: string;
}
/** Register a system directory without granting an Orb access to it. */
export function directoryFilesystem(path: string): DirectoryFilesystem {
  return Object.freeze({ kind: "directory", path });
}

export interface InterpreterOptions {
  engine: Uint8Array;
  providers?: {
    filesystem?: Readonly<Record<string, string | DirectoryFilesystem>>;
    network?: readonly string[];
  };
  limits?: { memoryBytes?: number; fuel?: number; packetBytes?: number; timeoutMs?: number };
  onLog?: (record: { orb: string; level: string; message: string }) => void;
}

/** A reusable backend; every installation materializes its own isolated heap and grant set. */
export class Interpreter {
  readonly #backend: NodeInterpreter;
  readonly #Contract: typeof NodeContract;

  /** Register providers independently of per-Orb permission decisions. */
  constructor(readonly options: InterpreterOptions) {
    const binding = createRequire(import.meta.url)(
      "@syntropika/tembra-engine-node",
    ) as typeof import("@syntropika/tembra-engine-node");
    this.#Contract = binding.NodeContract;
    this.#backend = new binding.NodeInterpreter(
      options.engine,
      JSON.stringify({
        limits: options.limits ?? {},
        network: options.providers?.network ?? [],
        filesystem: Object.fromEntries(
          Object.entries(options.providers?.filesystem ?? {}).map(([name, provider]) => [
            name,
            typeof provider === "string" ? provider : provider.path,
          ]),
        ),
      }),
      (packet) => {
        try {
          options.onLog?.(JSON.parse(packet));
        } catch {
          /* Observers cannot interrupt physical guest work. */
        }
      },
    );
  }

  /** Capture a closed source artifact; installation never downloads dependencies or executes host JS. */
  orb(options: InterpreterOrbOptions): OrbDefinition {
    const captured = JSON.parse(JSON.stringify(options)) as InterpreterOrbOptions;
    return Object.freeze({
      name: captured.name,
      apiVersion: captured.apiVersion,
      [materialize]: async () => {
        if (captured.source.format === "tembra-component-v1") {
          const source = captured.source;
          const bytes = Uint8Array.from(source.bytes);
          if (createHash("sha256").update(bytes).digest("hex") !== source.sha256)
            throw new Error("Prepared component digest differs");
          const port = this.#backend.openComponent(
            bytes,
            Uint8Array.from(source.contract),
            JSON.stringify({
              name: captured.name,
              apiVersion: captured.apiVersion,
              world: source.world,
              bindings: captured.bindings ?? {},
              grants: captured.grants ?? {},
            }),
          );
          try {
            return componentDefinition(port, captured.name, captured.apiVersion);
          } catch (error) {
            port.close();
            throw error;
          }
        }
        const port = this.#backend.open(
          captured.name,
          captured.apiVersion,
          JSON.stringify(captured.source),
          JSON.stringify(captured.bindings ?? {}),
          JSON.stringify(captured.grants ?? {}),
        );
        try {
          const contract = new this.#Contract(
            Uint8Array.from(captured.source.contract),
            captured.source.world,
          );
          return new Revision(port, contract, this.options.limits?.timeoutMs ?? 10000).definition(
            captured,
          );
        } catch (error) {
          port.close();
          throw error;
        }
      },
    } as AdaptedOrb);
  }

  /** Count actual Wasmtime Stores so tests can verify physical revision disposal. */
  instances(): number {
    return this.#backend.instances();
  }
}

/** Supply Tembra's isolated execution backend without exposing Wasmtime configuration to consumers. */
export function createInterpreter(options: InterpreterOptions): Interpreter {
  return new Interpreter(options);
}
