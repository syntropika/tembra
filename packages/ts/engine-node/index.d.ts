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
  bind(binding: string, method: string, exports: boolean): number;
  validate(slot: number, arguments_: boolean, value: string): string;
  request(packet: string): string;
}

/** Resolved WIT used by optional sandbox platform adapters. */
export class NodeContract {
  constructor(directory: string, world?: string);
  /** Encoded WIT for type generation; the object retains the original import authority. */
  wit(): Uint8Array;
  request(packet: string): string;
  sourceArtifact(source: string, name: string, apiVersion: number): Uint8Array;
  componentArtifact(
    component: Uint8Array,
    name: string,
    apiVersion: number,
    contributes?: Readonly<Record<string, number>>,
    references?: Readonly<Record<string, string>>,
  ): Uint8Array;
}

/** One generated core module instrumented with shared `tembra:limits` fuel and page globals. */
export interface BrowserCore {
  bytes: Buffer;
  /** Pages every instantiation of the module allocates for the memories it defines. */
  initialPages: number;
}

/**
 * Build-time browser preparation: instrument a generated core module. `maximumPages` (at most
 * 65536) clamps every defined memory; the runtime budget is the shared `pages` global.
 */
export function instrumentCore(bytes: Uint8Array, maximumPages: number): BrowserCore;

/** Build-time browser preparation: the exact versioned top-level import names of a component. */
export function componentImports(component: Uint8Array): string[];

/** Isolated Wasmtime backend used by Tembra's optional interpreter facade. */
export class NodeInterpreter {
  constructor(configuration: string, log: (packet: string) => void);
  interpreter(bytes: Uint8Array): void;
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
