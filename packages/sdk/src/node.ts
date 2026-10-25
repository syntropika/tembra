import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";
import path from "node:path";
import { Host } from "./host.ts";
import { facet } from "./facet.ts";
import type { HostOptions } from "./types.ts";
import type { KernelPort } from "./kernel.ts";

export type NodeHostOptions = Omit<HostOptions, "kernel"> & { kernel?: KernelPort };

/** Uses the existing Node runtime; it never embeds a second JavaScript engine. */
export function createHost(options: NodeHostOptions = {}): Host {
  const require = createRequire(import.meta.url);
  const binding = options.kernel
    ? undefined
    : (require(new URL("../dist/native/tembra.node", import.meta.url).pathname) as {
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
        if (!/\.m?js$/.test(url.pathname))
          throw new Error("Load a prebuilt JavaScript facet bundle");
        return facet((await import(url.href)).default as import("./types.ts").FacetDefinition);
      }),
  });
}

export { facet };
export type { Host };

export { Code, Reason, Lifetime, Status, ReloadStatus, HookCapability } from "./identifiers.ts";

export { Errors, type ErrorDescriptor } from "./errors.ts";
