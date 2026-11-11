import type { Buffer } from "node:buffer";

export class NodeKernel {
  check(root: number, call?: number): string | null;
  /** Execute one closed kernel operation on structured input; returns `{ ok }` or `{ error }`. */
  command(op: string, input: object): unknown;
}

/** Resolve WIT and its bundled standard contracts for build-time type generation. */
export class NodeArtifact {
  constructor(bytes: Uint8Array);
  description(): string;
  /** Keep the checked contract after its component has been materialized. */
  releaseComponent(): void;
  bind(binding: string, method: string, exports: boolean): number;
  validate(slot: number, arguments_: boolean, value: string): string;
  request(packet: string): string;
}

/** Resolved WIT used by optional sandbox platform adapters. */
export class NodeContract {
  /** Derive the world-independent interpreter core from the pinned authoring dylib. */
  static sourceRuntime(runtime: Uint8Array): Buffer;
  constructor(directory: string, world?: string);
  /** Encoded WIT for type generation; the object retains the original import authority. */
  wit(): Uint8Array;
  request(packet: string): string;
  sourceArtifact(
    source: string,
    runtime: Uint8Array,
    name: string,
    apiVersion: number,
    hooks?: string[],
  ): Uint8Array;
  componentArtifact(
    component: Uint8Array,
    name: string,
    apiVersion: number,
    contributes?: Readonly<Record<string, number>>,
  ): Uint8Array;
}

/** Wasmtime component backend behind Tembra's isolated execution facade. */
export class NodeInterpreter {
  constructor(configuration: string, log: (packet: string) => void);
  open(artifact: NodeArtifact, bindings: string, grants: string, cli?: string): NodeInterpreterPort;
  instances(): number;
}

/** One HTTP field, preserving value bytes, ordering and duplicate names. */
export interface NodeField {
  name: string;
  value: Buffer;
}

/** Incoming request metadata; body chunks travel separately on demand.
 * Native request targets supply scheme and authority together, or omit both.
 */
export interface NodeIngressHead {
  method: string;
  scheme?: string;
  authority?: string;
  pathWithQuery?: string;
  fields: NodeField[];
}

/** Typed carrier events; JSON appears only in control requests and completion packets. */
export type NodeIngressEvent =
  | { kind: "host"; id: number; asynchronous: boolean; packet: string }
  | { kind: "complete"; result: string }
  | { kind: "head"; status: number; fields: NodeField[] }
  | { kind: "request-pull" }
  | { kind: "data"; data: Buffer }
  | { kind: "trailers"; fields: NodeField[] }
  | { kind: "end" }
  | { kind: "error"; message: string }
  /** One standard binary WIT `wasi:http/types.error-code` value. */
  | { kind: "refused"; code: Buffer };

/** Scoped source or compiled Orb transport; application consumers should use the SDK facade. */
export class NodeInterpreterPort {
  /** Cancel the active callback after its caller was cancelled. */
  cancel(): void;
  release(environment: number): void;
  description(): string;
  declare(packet: string, callback: (packet: string) => string): string;
  dispatch(packet: string, callback: (packet: string) => void): void;
  /** Start an incoming exchange; finish only after its complete event. */
  ingress(packet: string, head: NodeIngressHead, callback: (event: NodeIngressEvent) => void): void;
  /** Supply exactly one of a body chunk, trailers ([] for EOF), or an error. */
  supplyRequest(
    data: Uint8Array | null,
    trailers: NodeField[] | null,
    error: string | null,
  ): boolean;
  /** Pull one response frame, preserving backpressure. */
  pullResponse(): void;
  /** Close both bodies when the client disconnects. */
  disconnect(): void;
  reply(id: number, packet: string): boolean;
  finish(): void;
  close(): void;
}
