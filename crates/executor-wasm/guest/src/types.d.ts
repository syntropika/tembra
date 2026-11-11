// Heterogeneous callbacks are kept inside one VM. Only bounded JSON crosses the host boundary.
type GuestCallback = (...arguments_: any[]) => any;
type GuestObject = Record<string | symbol, any>;
type WitType = string | number;
interface WitFunction {
  params: { name: string; type: WitType }[];
  result?: WitType;
}
interface WitInterface {
  binding: string;
  contract?: string;
  type_only?: boolean;
  functions: Record<string, WitFunction>;
}
interface WitKind {
  type?: WitType;
  list?: WitType;
  option?: WitType;
  /** The element type of a `stream`, or `null` for a stream without elements. */
  stream?: WitType | null;
  record?: { fields: { name: string; type: WitType }[] };
  tuple?: { types: WitType[] };
  variant?: { cases: { name: string; type?: WitType }[] };
  result?: { ok?: WitType; err?: WitType };
  flags?: unknown;
  enum?: unknown;
}
interface WitGraph {
  javascriptNames: Record<string, string>;
  types: { id: number; definition: { kind: WitKind } }[];
  imports: WitInterface[];
  exports: WitInterface[];
}
interface GuestValues {
  hasImportMethod(service: string, method: unknown): boolean;
  arguments(service: string, method: string, arguments_: unknown[]): unknown[];
  exportArguments(service: string, method: string, arguments_: unknown[]): unknown[];
  exportResult(service: string, method: string, value: unknown): unknown;
  /** Whether the export's whole result is a `stream<u8>`. */
  streamResult(service: string, method: string): boolean;
  /** Whether the import's whole result is a `stream<u8>`. */
  importStream(service: string, method: string): boolean;
  invoke(service: string, method: string, callback: () => unknown): Promise<unknown>;
  importArguments(service: string, method: string, arguments_: unknown[]): unknown[];
  importResult(service: string, method: string, value: unknown): unknown;
}
type Json = null | boolean | number | string | readonly Json[] | { readonly [key: string]: Json };
/** `value` is omitted only when the read named this same retained revision. */
interface Snapshot {
  value?: Json;
  revision: string;
  version: number;
}
interface GuestReadable {
  readonly value: Json;
  select(compute: (value: Json) => Json): GuestReadable;
  subscribe(): never;
}
interface GuestState {
  protect(value: unknown, ancestors?: Set<object>, depth?: number): Json;
  readable(
    read: (known: string | undefined) => Snapshot,
  ): GuestReadable & { adopt(snapshot: Snapshot): Json };
}
/** Handle issued by one environment table; `scope` selects the table, never an authority. */
interface HostHandle {
  scope?: number;
  id: number;
}
/** Synchronous host operations; mirrors `crates/executor-wasm/src/protocol.rs` (`console` is emitted by a registered native). */
interface HostRequests {
  address: {};
  orb: {};
  "signal.cancelled": {};
  "context.check": {};
  "context.principal": {};
  "context.cancel": {};
  "state.declare": { initial: unknown; version: number; validate: number | null };
  "state.read": HostHandle & { known?: string };
  "state.update": HostHandle & { value: unknown };
  "reference.declare": { service: string; key: string[] };
  "reference.release": HostHandle;
  "reference.status": HostHandle;
  "reference.state": HostHandle & { known?: string };
  "resource.declare": {
    name: string;
    version: number;
    open: number;
    close: number;
    dependencies: HostHandle[];
  };
  "resource.read": HostHandle;
  "hook.declare": { name: string };
}
/** Asynchronous host operations awaited by the outer driver. */
interface AsyncHostRequests {
  "hook.run": HostHandle & { value: unknown };
  "reference.ready": HostHandle;
  /** `Context.get`: issued into the calling environment's own table, so it names no scope. */
  "reference.acquire": { service: string; key: string[] };
  "reference.call": HostHandle & { method: string; args: unknown[] };
  wait: { milliseconds: number };
}
/** Omit the input argument only for operations without fields. */
type HostInput<T> = {} extends T ? [input?: T] : [input: T];
declare var __tembraContract: WitGraph;
declare var __tembraBuiltins: { clock: boolean; seed: [string, string] | null } | undefined;
declare var __tembraValues: GuestValues;
declare var __tembraState: GuestState;
declare var __tembraOrb: {
  name?: string;
  apiVersion?: number;
  create?: GuestCallback;
  services: Record<
    string,
    {
      create?: GuestCallback;
      methods: Record<string, GuestCallback>;
      lifetime?: string;
      linger?: number;
      access?: GuestCallback;
    }
  >;
  hooks?: GuestObject;
  contributes?: GuestObject;
};
declare var __hostSync: (packet: string) => string;
declare var __hostAsync: {
  (packet: string): Promise<string>;
  (packet: Uint8Array): Promise<Uint8Array>;
};
declare var __hostSyncBytes: ((packet: Uint8Array) => Uint8Array) | undefined;
/** Generated standard WASI modules for the imported interfaces, keyed by WIT interface. */
declare var __tembraStandard: Readonly<Record<string, Readonly<Record<string, unknown>>>>;
/** Safe-boundary standard cleanup; `ending` also closes the callback's input pumps. */
declare var __tembraSweepStandard: ((ending?: boolean) => void) | undefined;
declare var __scopeActive: (() => boolean) | undefined;
declare var __writeLog: ((level: string, message: string) => void) | undefined;
declare var __tembraTakeEffects: () => { id: number; packet: string | Uint8Array }[];
declare var __tembraCompleteEffect: (id: number, ok: boolean, value: string | Uint8Array) => void;
declare var __tembraStopEffects: () => void;
declare var __tembraPendingDispatch: unknown;
declare var __tembraAbandon: (() => void) | undefined;
declare var __tembraDescribe: () => string;
declare var __tembraDeclare: (packet: string) => string;
declare var __tembraDispatch: (packet: string) => Promise<string>;
/** Pending host operations of the current callback; set by the effect bridge. */
declare var __tembraEffects:
  | { readonly pending: number; readonly sequence: number; idle(): Promise<void> }
  | undefined;
/** Publish an export's whole `stream<u8>` result and produce it until delivered or closed. */
declare var __tembraStreamResult: ((source: unknown) => Promise<void>) | undefined;
/** Call an import whose whole result is a `stream<u8>`; resolves to the host-issued reader. */
declare var __tembraStreamImport: ((request: string) => Promise<unknown>) | undefined;
/** Incoming HTTP: decode the host-issued request, run the handler, publish and settle. */
declare var __tembraIngress:
  | { serve(request: number, handler: (request: unknown) => unknown): Promise<void> }
  | undefined;

declare var tembra: {
  wait(milliseconds: number): Promise<void>;
};
