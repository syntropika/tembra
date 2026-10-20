import * as sdk from "@tembra/sdk";
import * as sdkBrowser from "@tembra/sdk/browser";

import * as browser from "./browser.js";
import * as tooling from "./index.js";

let registered = false;

/** Trusted configuration uses the executable's canonical package identities without installation. */
export function registerEmbeddedPackages(): void {
  if (registered) {
    return;
  }
  Bun.plugin({
    name: "tembra-executable-packages",
    setup(builder) {
      builder.module("@tembra/tooling", () => ({ loader: "object", exports: tooling }));
      builder.module("@tembra/tooling/browser", () => ({ loader: "object", exports: browser }));
      builder.module("@tembra/sdk", () => ({ loader: "object", exports: sdk }));
      builder.module("@tembra/sdk/browser", () => ({ loader: "object", exports: sdkBrowser }));
    },
  });
  registered = true;
}
