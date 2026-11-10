import type { HostOptions, OrbArtifact, OrbDefinition } from "../authoring/types.ts";
import { Host } from "../host.ts";
import { isolate, type AdaptedHostOptions } from "#tembra-adapters";
import { Revision } from "./revision.ts";
import { decodeAsyncHostRequest, decodeHostRequest } from "./host-protocol.ts";
import { reply } from "./codec.ts";
import { cliPacket } from "./cli.ts";
import type { IngressExchange } from "./http-ingress.ts";
import type { HttpField } from "./http-provider.ts";
import { ingressPacket, NativeIngress, type CarrierEvent } from "./native-ingress.ts";
import {
  answer,
  query,
  type ContractPort,
  type DirectoryFilesystem,
  type ExecutionScope,
  type IngressDispatch,
  type InstallationPlan,
  type InterpreterPort,
} from "./types.ts";

/** Platform-owned objects retain all native handles behind the mobile facade. */
export interface MobileArtifact extends ContractPort {
  description(): string;
  free(): void;
}
/**
 * The native UniFFI incoming HTTP carrier (`begin_ingress`, `receive_ingress`, `supply_request`,
 * `pull_response`, `disconnect`) in lowerCamelCase. Bytes are typed `Uint8Array`s end to end:
 * a bridge that cannot carry binary data (number arrays, base64) must not provide this group.
 */
export interface MobileField {
  name: string;
  value: Uint8Array;
}
export interface MobileRequestHead {
  method: string;
  /** Present exactly when `authority` is. */
  scheme?: string;
  authority?: string;
  pathWithQuery?: string;
  fields: MobileField[];
}
/** The answer to one `requestPull`. */
export type MobileRequestChunk =
  | { kind: "data"; bytes: Uint8Array }
  /** The end of the request body; `[]` for no trailers. */
  | { kind: "end"; trailers: MobileField[] }
  | { kind: "error"; message: string };
export type MobileIngressEvent =
  | { kind: "host"; id: number; asynchronous: boolean; packet: string }
  /** The completion envelope JSON; `finish` follows. */
  | { kind: "complete"; result: string }
  | { kind: "head"; status: number; fields: MobileField[] }
  | { kind: "requestPull" }
  | { kind: "data"; bytes: Uint8Array }
  | { kind: "trailers"; fields: MobileField[] }
  | { kind: "end" }
  | { kind: "error"; message: string }
  /** The handler's WIT `error-code` in the standard binary codec; never with a head. */
  | { kind: "refused"; code: Uint8Array };
/** The whole carrier, or nothing: a port either serves incoming HTTP or does not. */
export interface MobileIngressCarrier {
  /** Start the exchange; its events then come only from `receiveIngress`. */
  beginIngress(packet: string, head: MobileRequestHead): void;
  /** The single receive loop of the active exchange. */
  receiveIngress(): Promise<MobileIngressEvent>;
  /** `false` when the answer is no longer wanted. */
  supplyRequest(chunk: MobileRequestChunk): boolean;
  pullResponse(): void;
  disconnect(): void;
}
export interface MobilePort {
  description(): string;
  begin(packet: string, declaration: boolean): void;
  receive(): Promise<string>;
  reply(id: number, packet: string): boolean;
  finish(): void;
  abort(): Promise<void>;
  release(environment: number): void;
  /** Cancel standard WASI work owned by the active callback. */
  cancel(): void;
  close(): void;
  /**
   * The typed incoming HTTP carrier, absent where the platform bridge cannot carry raw bytes;
   * `Host.handleHttp` is then refused for this port's Orbs.
   */
  readonly ingress?: MobileIngressCarrier | undefined;
}
export interface MobileBackend {
  artifact(bytes: Uint8Array): MobileArtifact;
  /** Open a source or compiled revision; both answer the same scoped host requests. */
  open(
    artifact: MobileArtifact,
    bindings: string,
    grants: string,
    cli: string,
  ): Promise<MobilePort>;
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
  /** Present only when the port provides the complete typed carrier. */
  declare readonly ingress?: NonNullable<InterpreterPort["ingress"]>;
  constructor(
    readonly port: MobilePort,
    readonly dispose: () => void,
  ) {
    const carrier = port.ingress;
    if (carrier)
      this.ingress = (dispatch, exchange, scope) => this.serve(carrier, dispatch, exchange, scope);
  }
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

  /**
   * Serve one incoming HTTP request over the typed carrier: one receive loop feeds the shared
   * exchange protocol until `complete`; a broken receive ends both bodies, drains admitted host
   * work and quarantines the native exchange, exactly as an ordinary callback does.
   */
  private async serve(
    carrier: MobileIngressCarrier,
    dispatch: IngressDispatch,
    exchange: IngressExchange,
    scope: ExecutionScope,
  ): Promise<void> {
    const port = this.port;
    const exchanged = new NativeIngress(
      {
        supply: (answer) =>
          carrier.supplyRequest(
            answer.kind === "data"
              ? { kind: "data", bytes: answer.data }
              : answer.kind === "end"
                ? { kind: "end", trailers: answer.trailers.map(mobileField) }
                : answer,
          ),
        pull: () => carrier.pullResponse(),
        disconnect: () => carrier.disconnect(),
        cancel: () => port.cancel(),
        reply: (id, packet) => void port.reply(id, packet),
        finish: () => port.finish(),
      },
      exchange,
      scope,
    );
    const { method, scheme, authority, pathWithQuery, fields } = exchange.request;
    try {
      carrier.beginIngress(ingressPacket(dispatch), {
        method,
        ...(scheme === undefined ? {} : { scheme }),
        ...(authority === undefined ? {} : { authority }),
        ...(pathWithQuery === undefined ? {} : { pathWithQuery }),
        fields: fields.map(mobileField),
      });
    } catch (error) {
      // Refused before admission: nothing native began.
      await exchanged.abandon(error);
      return exchanged.settled;
    }
    exchanged.begun();
    try {
      for (;;) {
        const received = await carrier.receiveIngress();
        let event: CarrierEvent;
        try {
          event = carrierEvent(received);
        } catch (error) {
          // A malformed frame fails this call (disconnect, cancel); the exchange still completes.
          exchanged.fail(error);
          continue;
        }
        exchanged.event(event);
        if (event.kind === "complete") break;
      }
    } catch (error) {
      await exchanged.abandon(error);
      const aborted = await Promise.allSettled([port.abort()]);
      if (aborted[0]?.status === "rejected")
        throw new AggregateError([error, aborted[0].reason], "Mobile exchange and cleanup failed");
      throw error;
    }
    return exchanged.settled;
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
    const signal = scope.signal;
    const cancel = () => this.port.cancel();
    const detach = () => signal?.removeEventListener("abort", cancel);
    try {
      // Standard WASI calls stay inside native execution and emit no SDK host request.
      if (signal?.aborted) cancel();
      else signal?.addEventListener("abort", cancel, { once: true });
      for (;;) {
        const event = JSON.parse(await this.port.receive());
        if (event.event === "complete") {
          detach();
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
      detach();
      scope.cancel();
      const results = await Promise.allSettled([this.port.abort(), ...pending]);
      const abort = results[0];
      if (abort?.status === "rejected")
        throw new AggregateError([error, abort.reason], "Mobile exchange and cleanup failed");
      throw error;
    } finally {
      detach();
    }
  }
}

function mobileField([name, value]: HttpField): MobileField {
  return { name, value };
}

/** Only typed bytes cross; an array or string from a bridge is refused, never converted. */
function typed(value: unknown): Uint8Array {
  if (!(value instanceof Uint8Array))
    throw new TypeError("the mobile ingress carrier must deliver bytes as Uint8Array");
  return value;
}

function fieldsOf(value: readonly MobileField[]): readonly HttpField[] {
  if (!Array.isArray(value)) throw new TypeError("mobile ingress fields must be an array");
  return value.map((field) => {
    if (typeof field?.name !== "string")
      throw new TypeError("mobile ingress field names are strings");
    return [field.name, typed(field.value)] as const;
  });
}

/** The UniFFI event in the shared carrier form. */
function carrierEvent(event: MobileIngressEvent): CarrierEvent {
  switch (event.kind) {
    case "host":
    case "complete":
    case "end":
    case "error":
      return event;
    case "requestPull":
      return { kind: "request-pull" };
    case "head":
      return { kind: "head", status: event.status, fields: fieldsOf(event.fields) };
    case "data":
      return { kind: "data", data: typed(event.bytes) };
    case "trailers":
      return { kind: "trailers", fields: fieldsOf(event.fields) };
    case "refused":
      return { kind: "refused", code: typed(event.code) };
    default:
      throw new TypeError("Unknown mobile ingress event");
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
          http: isolated.providers?.network ?? [],
        });
        const bindings = JSON.stringify(installation.bindings),
          grants = JSON.stringify(definition.grants ?? {});
        const port = await backend.open(artifact, bindings, grants, cliPacket(definition.cli));
        const driver = new MobileDriver(port, () => artifact.free());
        try {
          return new Revision(driver, artifact, isolated.limits?.timeoutMs ?? 10000).definition(
            identity,
            installation,
          );
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
