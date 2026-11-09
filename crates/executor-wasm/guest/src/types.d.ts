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
  functions: Record<string, WitFunction>;
}
interface WitKind {
  type?: WitType;
  list?: WitType;
  option?: WitType;
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
}
/** Asynchronous host operations awaited by the outer driver. */
interface AsyncHostRequests {
  "filesystem.read-text": { path: string };
  "filesystem.write-text": { path: string; value: string };
  "transport.exchange": { ip: string; port: number; request: string };
  "reference.ready": HostHandle;
  "reference.call": HostHandle & { method: string; args: unknown[] };
  wait: { milliseconds: number };
}
/** Omit the input argument only for operations without fields. */
type HostInput<T> = {} extends T ? [input?: T] : [input: T];
declare var __tembraContract: WitGraph;
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
declare var __hostAsync: (packet: string) => Promise<string>;
declare var __scopeActive: (() => boolean) | undefined;
declare var __writeLog: ((level: string, message: string) => void) | undefined;
declare var __tembraTakeEffects: () => { id: number; packet: string }[];
declare var __tembraCompleteEffect: (id: number, ok: boolean, value: string) => void;
declare var __tembraStopEffects: () => void;
declare var __tembraPendingDispatch: unknown;
declare var __tembraAbandon: (() => void) | undefined;
declare var __tembraDescribe: () => string;
declare var __tembraDeclare: (packet: string) => string;
declare var __tembraDispatch: (packet: string) => Promise<string>;

declare var tembra: {
  wait(milliseconds: number): Promise<void>;
  filesystem?: {
    readText(path: string): Promise<string>;
    writeText(path: string, value: string): Promise<void>;
  };
  transport?: { exchange(ip: string, port: number, request: string): Promise<string> };
};
