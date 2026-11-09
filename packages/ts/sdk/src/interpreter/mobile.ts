import type { HostOptions, OrbArtifact, OrbDefinition } from "../authoring/types.ts";
import { Host } from "../host.ts";
import { isolate, type AdaptedHostOptions } from "#tembra-adapters";
import { Revision } from "./revision.ts";
import { decodeAsyncHostRequest, decodeHostRequest } from "./host-protocol.ts";
import { reply } from "./codec.ts";
import {
  answer,
  query,
  type ContractPort,
  type DirectoryFilesystem,
  type ExecutionScope,
  type InstallationPlan,
  type InterpreterPort,
} from "./types.ts";

/** Platform-owned objects retain all native handles behind the mobile facade. */
export interface MobileArtifact extends ContractPort {
  description(): string;
  free(): void;
}
export interface MobilePort {
  description(): string;
  begin(packet: string, declaration: boolean): void;
  receive(): Promise<string>;
  reply(id: number, packet: string): boolean;
  finish(): void;
  abort(): Promise<void>;
  release(environment: number): void;
  io(packet: string): Promise<string>;
  close(): void;
}
export interface MobileBackend {
  artifact(bytes: Uint8Array): MobileArtifact;
  /** Open a source or compiled revision; both answer the same scoped host requests. */
  open(artifact: MobileArtifact, bindings: string, grants: string): Promise<MobilePort>;
}
export interface MobileExecutionOptions {
  onLog?: (record: { orb: string; level: string; message: string }) => void;
  providers?: {
    filesystem?: Readonly<Record<string, DirectoryFilesystem>>;
    network?: readonly string[];
  };
  limits?: { memoryBytes?: number; fuel?: number; packetBytes?: number; timeoutMs?: number };
}

/** Adapt asynchronous platform delivery while guest factories remain strictly synchronous. */
export class MobileDriver implements InterpreterPort {
  constructor(
    readonly port: MobilePort,
    readonly dispose: () => void,
  ) {}
  /** Return already checked executable metadata. */
  description(): string {
    return this.port.description();
  }
  /** Drop guest bookkeeping only after the environment has drained. */
  release(environment: number): void {
    this.port.release(environment);
  }
  /** Keep declaration requests attached to their issued factory authority. */
  async declare(packet: string, scope: ExecutionScope): Promise<unknown> {
    return JSON.parse(await this.drive(packet, scope, true));
  }
  /** Retain captured result bytes until the shared contract validator checks them. */
  execute(packet: string, scope: ExecutionScope): Promise<string> {
    return this.drive(packet, scope, false);
  }
  /** Release the native revision and its prepared artifact together. */
  close(): void {
    try {
      this.port.close();
    } finally {
      this.dispose();
    }
  }

  /** Drain every admitted host operation before reopening the physical guest ticket. */
  private async drive(
    packet: string,
    scope: ExecutionScope,
    declaration: boolean,
  ): Promise<string> {
    this.port.begin(packet, declaration);
    const pending = new Set<Promise<void>>();
    let failure: unknown;
    let finished = false;
    try {
      for (;;) {
        const event = JSON.parse(await this.port.receive());
        if (event.event === "complete") {
          scope.cancel();
          await Promise.allSettled([...pending]);
          this.port.finish();
          finished = true;
          if (failure !== undefined) throw failure;
          const value = answer<unknown>(JSON.stringify(event.result));
          if (typeof value !== "string") throw new Error("Invalid guest result packet");
          return value;
        }
        if (event.event !== "host") throw new Error("Unknown interpreter event");
        const respond = (operation: () => unknown): void => {
          let response: string;
          try {
            response = `{"ok":${reply(operation())}}`;
          } catch (error) {
            response = JSON.stringify({
              error: error instanceof Error ? error.message : String(error),
            });
          }
          this.port.reply(event.id, response);
        };
        if (!event.asynchronous) respond(() => scope.sync(decodeHostRequest(event.packet)));
        else if (declaration)
          respond(() => {
            throw new Error("Factories cannot perform asynchronous host operations");
          });
        else {
          const operation = Promise.resolve()
            .then(() => scope.async(decodeAsyncHostRequest(event.packet)))
            .then(
              (value) => respond(() => value),
              (error) =>
                respond(() => {
                  throw error;
                }),
            )
            .catch((error) => {
              failure = error;
            });
          pending.add(operation);
          void operation.finally(() => pending.delete(operation));
        }
      }
    } catch (error) {
      if (finished) throw error;
      scope.cancel();
      const results = await Promise.allSettled([this.port.abort(), ...pending]);
      const abort = results[0];
      if (abort?.status === "rejected")
        throw new AggregateError([error, abort.reason], "Mobile exchange and cleanup failed");
      throw error;
    }
  }
}

/** Connect native execution to the existing host without copying lifecycle or permission rules. */
export function createMobileHost(
  options: HostOptions,
  backend: MobileBackend,
  isolated: MobileExecutionOptions,
): Host {
  return new Host({
    ...options,
    [isolate]: async (definition: OrbArtifact): Promise<OrbDefinition> => {
      const artifact = backend.artifact(definition.artifact);
      try {
        const identity = JSON.parse(artifact.description()) as {
          name: string;
          apiVersion: number;
          kind: string;
        };
        const installation = query<InstallationPlan>(artifact, {
          kind: "install",
          name: identity.name,
          apiVersion: identity.apiVersion,
          requires: definition.requires ?? {},
          grants: definition.grants ?? {},
          filesystem: Object.keys(isolated.providers?.filesystem ?? {}),
          network: isolated.providers?.network ?? [],
        });
        const bindings = JSON.stringify(installation.bindings),
          grants = JSON.stringify(definition.grants ?? {});
        const port = await backend.open(artifact, bindings, grants);
        const driver = new MobileDriver(port, () => artifact.free());
        try {
          return new Revision(driver, artifact, isolated.limits?.timeoutMs ?? 10000, (request) =>
            port.io(JSON.stringify(request)).then((value) => JSON.parse(value)),
          ).definition(identity, installation);
        } catch (error) {
          driver.close();
          throw error;
        }
      } catch (error) {
        artifact.free();
        throw error;
      }
    },
  } as AdaptedHostOptions);
}
