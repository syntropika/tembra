import { createRequire } from "node:module";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { orb } from "./authoring/orb.ts";
import type { HostOptions } from "./authoring/types.ts";
import type { KernelPort } from "./bridge/kernel.ts";
import { Host } from "./host.ts";

export type NodeHostOptions = Omit<HostOptions, "kernel"> & { kernel?: KernelPort };

/** Uses the existing Node runtime; it never embeds a second JavaScript engine. */
export function createHost(options: NodeHostOptions = {}): Host {
  const require = createRequire(import.meta.url);
  const binding = options.kernel
    ? undefined
    : (require("@tembra/engine-node") as {
        NodeKernel: new () => KernelPort;
      });
  return new Host({
    ...options,
    kernel: options.kernel ?? new binding!.NodeKernel(),
    load:
      options.load ??
      (async (location) => {
        const url =
          location instanceof URL
            ? location
            : /^[a-z]+:/i.test(location)
              ? new URL(location)
              : pathToFileURL(path.resolve(location));
        if (!/\.m?js$/.test(url.pathname)) throw new Error("Load a prebuilt JavaScript orb bundle");
        return orb(
          (await import(url.href)).default as import("./authoring/types.ts").OrbDefinition,
        );
      }),
  });
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
