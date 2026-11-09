import { createRequire } from "node:module";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { orb } from "./authoring/orb.ts";
import { readFile } from "node:fs/promises";
import { isolate, type AdaptedHostOptions } from "#tembra-adapters";
import { NodeExecution, type InterpreterOptions } from "./interpreter/node.ts";
import type { HostOptions } from "./authoring/types.ts";
import type { KernelPort } from "./bridge/kernel.ts";
import { Host } from "./host.ts";

export type NodeHostOptions = Omit<HostOptions, "kernel"> & {
  kernel?: KernelPort;
  isolated?: InterpreterOptions;
};

/** Configure the ordinary host and lazily create isolated execution for Wasm Orbs. */
export function createHost(options: NodeHostOptions = {}): Host {
  const require = createRequire(import.meta.url);
  let kernel = options.kernel;
  if (!kernel) {
    const { NodeKernel } = require("@syntropika/tembra-engine-node") as {
      NodeKernel: new () => KernelPort;
    };
    kernel = new NodeKernel();
  }
  let execution: NodeExecution | undefined;
  const backend = () => (execution ??= new NodeExecution(options.isolated ?? {}));
  return new Host({
    [isolate]: (definition) => backend().materialize(definition),
    ...options,
    kernel,
    load:
      options.load ??
      (async (location, registration) => {
        const url =
          location instanceof URL
            ? location
            : /^[a-z]+:/i.test(location)
              ? new URL(location)
              : pathToFileURL(path.resolve(location));
        if (!url.pathname.endsWith(".wasm"))
          throw new Error("Orb loading requires a built .wasm artifact");
        let source: Uint8Array;
        if (url.protocol === "file:") source = new Uint8Array(await readFile(url));
        else {
          const response = await fetch(url);
          if (!response.ok) throw new Error(`Orb request failed: ${response.status}`);
          source = new Uint8Array(await response.arrayBuffer());
        }
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

export { directoryFilesystem, type DirectoryFilesystem } from "./interpreter/node.ts";
export type { InterpreterOptions as IsolatedOptions } from "./interpreter/node.ts";
export type { Grants } from "./authoring/types.ts";
