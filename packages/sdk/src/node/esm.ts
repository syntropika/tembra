import { Buffer } from "node:buffer";

import type { ArtifactEvaluator } from "../host/artifact-runtime.js";

/** Native ESM implements the existing closed-module profile without rewriting its source. */
export function nodeEsmEvaluator(authority: string): ArtifactEvaluator {
  return Object.freeze({
    name: "node-esm",
    profile: "tembra-esm-v1",
    async evaluate(module, identity) {
      const url = `data:text/javascript;base64,${Buffer.from(module.artifact.source, "utf-8").toString("base64")}#tembra-${authority}-${identity}`;
      const namespace: unknown = await import(url);
      return namespace;
    },
  });
}
