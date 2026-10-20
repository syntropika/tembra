import { isManagedMessage } from "../manager/channel.js";
import { serveManagedHost } from "../manager/child.js";
import type { ManagedChannel } from "../manager/index.js";
import { createHost } from "./host.js";
// Worker entrypoints use the same event target and postMessage primitives in supported browsers.
// oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This module is exclusively the dedicated Worker bootstrap; the SDK DOM compilation excludes lib.webworker.
const scope = globalThis as unknown as {
  postMessage: (message: unknown) => void;
  addEventListener: (type: string, listener: (event: MessageEvent<unknown>) => void) => void;
  removeEventListener: (type: string, listener: (event: MessageEvent<unknown>) => void) => void;
  close: () => void;
};
let module: string | undefined;
let wasmURL: string | undefined;
let configured = false;
const channel: ManagedChannel = {
  send: (message) => {
    // oxlint-disable-next-line unicorn/require-post-message-target-origin -- Dedicated Worker postMessage targets its owning port.
    scope.postMessage(message);
  },
  onMessage(listener) {
    const handler = (event: MessageEvent<unknown>) => {
      const message = event.data;
      if (isManagedMessage(message)) {
        if (
          !configured &&
          message.type === "init" &&
          "module" in message &&
          typeof message.module === "string"
        ) {
          configured = true;
          ({ module } = message);
          if ("wasmURL" in message && typeof message.wasmURL === "string") {
            ({ wasmURL } = message);
          }
        }
        listener(message);
      }
    };
    scope.addEventListener("message", handler);
    return () => {
      scope.removeEventListener("message", handler);
    };
  },
  onFailure() {
    return () => {
      /* Parent port closure is detected by core RPC liveness in Workers. */
    };
  },
  // oxlint-disable-next-line typescript/require-await -- Uniform driver disposal is asynchronous; Worker global close is synchronous.
  async terminate() {
    scope.close();
  },
};
serveManagedHost(
  channel,
  async (name) =>
    await createHost({ name, ...(wasmURL === undefined ? {} : { wasmInput: wasmURL }) }),
  async (host) => {
    if (module === undefined || module === "") {
      throw new Error("Worker facet module missing");
    }
    await host.load(module);
  },
);
