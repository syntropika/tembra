import { isolate, type AdaptedHostOptions } from "#tembra-adapters";
import type { BrowserExecution, BrowserInterpreterOptions } from "./interpreter/browser.ts";
import { orb } from "./authoring/orb.ts";
import type { HostOptions } from "./authoring/types.ts";
import type { KernelPort } from "./bridge/kernel.ts";
import { Host } from "./host.ts";

export interface WasmModule {
  WasmKernel: new () => KernelPort;
  default?: (input?: unknown) => Promise<unknown>;
}
export type BrowserHostOptions = Omit<HostOptions, "kernel"> & {
  kernel?: KernelPort;
  isolated?: BrowserInterpreterOptions;
  wasm?: WasmModule | (() => Promise<WasmModule>);
  wasmInput?: unknown;
  baseURL?: string | URL;
};

/** Configure a browser host with lazily loaded isolated execution for Wasm Orbs. */
export async function createHost(options: BrowserHostOptions = {}): Promise<Host> {
  let kernel = options.kernel;
  if (!kernel) {
    const supplied = options.wasm;
    const module =
      typeof supplied === "function"
        ? await supplied()
        : (supplied ?? ((await import("@syntropika/tembra-engine-wasm")) as WasmModule));
    await module.default?.(options.wasmInput);
    kernel = new module.WasmKernel();
  }
  let execution: Promise<BrowserExecution> | undefined;
  const backend = () => {
    if (!execution) {
      const opening = import("./interpreter/browser.ts").then((module) =>
        module.createExecution(options.isolated ?? {}),
      );
      execution = opening;
      opening.catch(() => {
        if (execution === opening) execution = undefined;
      });
    }
    return execution;
  };
  return new Host({
    [isolate]: async (definition) => (await backend()).materialize(definition),
    ...options,
    kernel,
    load:
      options.load ??
      (async (location, registration) => {
        const base = options.baseURL ?? globalThis.location?.href;
        const url =
          location instanceof URL ? location : base ? new URL(location, base) : new URL(location);
        if (!url.pathname.endsWith(".wasm"))
          throw new Error("Orb loading requires a built .wasm artifact");
        const response = await fetch(url);
        if (!response.ok) throw new Error(`Orb request failed: ${response.status}`);
        const source = new Uint8Array(await response.arrayBuffer());
        return { ...registration, artifact: source };
      }),
  } as AdaptedHostOptions);
}

export { orb };
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

export {
  memoryFilesystem,
  opfsFilesystem,
  FilesystemError,
  type FilesystemErrorCode,
  type FilesystemProvider,
  type FileStat,
  type MountedFilesystem,
  type FileDescriptor,
  type DirectoryDescriptor,
  type Descriptor,
  type DirectoryEntry,
  type EntryType,
  type OpenOptions,
} from "./interpreter/filesystem.ts";
export type { BrowserInterpreterOptions as IsolatedOptions } from "./interpreter/browser.ts";
export type { Grants, CliOptions } from "./authoring/types.ts";

export type { HttpProvider, SocketProvider } from "./interpreter/browser-network.ts";
export type { ClockProvider, ClockInstant, RandomProvider } from "./interpreter/standard-system.ts";
export type {
  StdioProvider,
  StdioStreams,
  StdioInput,
  StdioOutput,
} from "./interpreter/standard-cli.ts";
