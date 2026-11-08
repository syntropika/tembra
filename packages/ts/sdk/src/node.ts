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
  const binding = options.kernel
    ? undefined
    : (require("@syntropika/tembra-engine-node") as {
        NodeKernel: new () => KernelPort;
      });
  let execution: NodeExecution | undefined;
  const backend = () => (execution ??= new NodeExecution(options.isolated ?? {}));
  return new Host({
    [isolate]: (definition) => backend().materialize(definition),
    ...options,
    kernel: options.kernel ?? new binding!.NodeKernel(),
    load:
      options.load ??
      (async (location, registration) => {
        const url =
          location instanceof URL
            ? location
            : /^[a-z]+:/i.test(location)
              ? new URL(location)
              : pathToFileURL(path.resolve(location));
        if (url.pathname.endsWith(".wasm")) {
          const source =
            url.protocol === "file:"
              ? new Uint8Array(await readFile(url))
              : new Uint8Array(await (await fetch(url)).arrayBuffer());
          return backend().decode(source, registration);
        }
        throw new Error("Orb loading requires a built .wasm artifact");
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
export type { Grants } from "./interpreter/types.ts";
