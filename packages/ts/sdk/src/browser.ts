import { facet } from "./authoring/facet.ts";
import type { HostOptions } from "./authoring/types.ts";
import type { KernelPort } from "./bridge/kernel.ts";
import { Host } from "./host.ts";

export interface WasmModule {
  WasmKernel: new () => KernelPort;
  default?: (input?: unknown) => Promise<unknown>;
}
export type BrowserHostOptions = Omit<HostOptions, "kernel"> & {
  kernel?: KernelPort;
  wasm?: WasmModule | (() => Promise<WasmModule>);
  wasmInput?: unknown;
  baseURL?: string | URL;
};

/** Runs facets in the browser or Worker's existing JS realm; WASM owns only kernel metadata. */
export async function createHost(options: BrowserHostOptions = {}): Promise<Host> {
  let kernel = options.kernel;
  if (!kernel) {
    const supplied = options.wasm;
    const module =
      typeof supplied === "function"
        ? await supplied()
        : (supplied ?? ((await import("@tembra/engine-wasm")) as WasmModule));
    await module.default?.(options.wasmInput);
    kernel = new module.WasmKernel();
  }
  return new Host({
    ...options,
    kernel,
    load:
      options.load ??
      (async (location) => {
        const base = options.baseURL ?? globalThis.location?.href;
        const url =
          location instanceof URL ? location : base ? new URL(location, base) : new URL(location);
        return facet(
          (await import(url.href)).default as import("./authoring/types.ts").FacetDefinition,
        );
      }),
  });
}

export { facet };
export type { Host };

export {
  Code,
  HookCapability,
  Lifetime,
  Reason,
  ReloadStatus,
  Status,
} from "./bridge/identifiers.ts";

export { Errors, type ErrorDescriptor } from "./errors.ts";

export { ContextKey, ContextKind, opaqueFrom, type OpaqueContextValue } from "./context-key.ts";
