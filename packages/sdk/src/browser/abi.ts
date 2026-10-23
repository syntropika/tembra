import type { RpcEndpoint } from "../manager/bridge.js";

/** Retains method variance at the structural boundary to generated WASM classes. */
export type WasmMethod<Arguments extends unknown[], Output> = {
  // oxlint-disable-next-line typescript/method-signature-style -- Generated classes have disposal-only members that must not become requirements on every issued structural handle.
  invoke(...arguments_: Arguments): Output;
}["invoke"];

/** Exact private wasm-bindgen handles shipped with this SDK. Rust owns their dispatch. */
export type WasmSubscription = {
  unsubscribe: WasmMethod<[], void>;
  free: WasmMethod<[], void>;
};
export type WasmConnection = {
  ready: WasmMethod<[], Promise<void>>;
  closed: WasmMethod<[], Promise<string>>;
  close: WasmMethod<[], void>;
  free: WasmMethod<[], void>;
  status: WasmMethod<[], string>;
};
export type WasmHost = {
  rpcIdentity: WasmMethod<[incarnation: string], void>;
  rpcAttach: WasmMethod<[peer: string, child: boolean], RpcEndpoint>;
  connect: WasmMethod<[url: string, provider: string], WasmConnection>;
  cancelContext: WasmMethod<[token: unknown], void>;
  close: WasmMethod<[], Promise<void>>;
  errorsJson: WasmMethod<[], string>;
  observationsJson: WasmMethod<[limit: number], string>;
  free: WasmMethod<[], void>;
};
export type WasmModule = {
  WasmHost: { withOptions: WasmMethod<[name: string, options: string], WasmHost> };
};
