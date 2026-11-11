import { NodePort } from "./node-port.ts";
import { cliPacket } from "./cli.ts";
import { createRequire } from "node:module";
import type { NodeArtifact, NodeInterpreter } from "@syntropika/tembra-engine-node";
import type { OrbDefinition, OrbArtifact, LoadOptions } from "../authoring/types.ts";
import { Revision } from "./revision.ts";
import { query, type InstallationPlan, type DirectoryFilesystem } from "./types.ts";
export type { DirectoryFilesystem } from "./types.ts";

/** Register a system directory without granting an Orb access to it. */
export function directoryFilesystem(path: string): DirectoryFilesystem {
  return Object.freeze({ kind: "directory", path });
}

export interface InterpreterOptions {
  providers?: {
    filesystem?: Readonly<Record<string, DirectoryFilesystem>>;
    network?: readonly string[];
  };
  limits?: {
    memoryBytes?: number;
    fuel?: number;
    packetBytes?: number;
    timeoutMs?: number;
  };
  onLog?: (record: { orb: string; level: string; message: string }) => void;
}

/** Platform transport owns Stores; the common host still owns every lifecycle decision. */
export class NodeExecution {
  #backend: NodeInterpreter | undefined;
  readonly #binding = createRequire(import.meta.url)(
    "@syntropika/tembra-engine-node",
  ) as typeof import("@syntropika/tembra-engine-node");

  /** Register host configuration; component code is compiled only when installed. */
  constructor(readonly options: InterpreterOptions) {}

  /** Materialize a verified component supplied through an Orb declaration. */
  async materialize(definition: OrbArtifact): Promise<OrbDefinition> {
    return this.open(new this.#binding.NodeArtifact(definition.artifact), definition);
  }

  /** Lazily share compiled engine code while keeping provider tables local to this host. */
  private backend(): NodeInterpreter {
    if (this.#backend) return this.#backend;
    this.#backend = new this.#binding.NodeInterpreter(
      JSON.stringify({
        limits: this.options.limits ?? {},
        network: this.options.providers?.network ?? [],
        filesystem: Object.fromEntries(
          Object.entries(this.options.providers?.filesystem ?? {}).map(([name, provider]) => [
            name,
            provider.path,
          ]),
        ),
      }),
      (packet) => {
        try {
          this.options.onLog?.(JSON.parse(packet));
        } catch {
          /* Observers cannot interrupt physical guest work. */
        }
      },
    );
    return this.#backend;
  }

  /** Resolve grants once, then adapt the opaque artifact through the common lifecycle. */
  private async open(artifact: NodeArtifact, options: LoadOptions): Promise<OrbDefinition> {
    const identity = JSON.parse(artifact.description()) as {
      name: string;
      apiVersion: number;
      kind: "component";
    };
    const prepared = query<InstallationPlan>(artifact, {
      kind: "install",
      name: identity.name,
      apiVersion: identity.apiVersion,
      requires: options.requires ?? {},
      grants: options.grants ?? {},
      filesystem: Object.keys(this.options.providers?.filesystem ?? {}),
      network: this.options.providers?.network ?? [],
      http: this.options.providers?.network ?? [],
    });
    const backend = this.backend();
    const port = backend.open(
      artifact,
      JSON.stringify(prepared.bindings),
      JSON.stringify(options.grants ?? {}),
      cliPacket(options.cli),
    );
    try {
      const transport = new NodePort(port);
      return new Revision(transport, artifact, this.options.limits?.timeoutMs ?? 10000).definition(
        identity,
        prepared,
      );
    } catch (error) {
      port.close();
      throw error;
    }
  }
}
