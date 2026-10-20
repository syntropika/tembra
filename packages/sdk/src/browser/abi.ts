/** Retains method variance at the structural boundary to generated WASM classes. */
export type WasmMethod<Arguments extends unknown[], Output> = {
  // oxlint-disable-next-line typescript/method-signature-style -- Generated classes have disposal-only members that must not become requirements on every issued structural handle.
  invoke(...arguments_: Arguments): Output;
}["invoke"];
