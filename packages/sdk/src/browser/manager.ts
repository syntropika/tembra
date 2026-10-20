import { isManagedMessage } from "../manager/channel.js";
import { createManagedHostManager } from "../manager/index.js";
import type {
  HostDriver,
  HostManager,
  HostManagerOptions,
  ManagedChannel,
  ManagedMessage,
} from "../manager/index.js";

export type {
  HostManager,
  HostInstance,
  HostDriver,
  HostManagerOptions,
  ManagedChannel,
  ManagedMessage,
} from "../manager/index.js";
export type WebWorkerDriverOptions = {
  module: string | URL;
  /** Explicit bootstrap URL when the application bundles Worker assets. */
  workerURL?: string | URL;
  /** Explicit packaged WASM asset URL when required by the deployment. */
  wasmURL?: string | URL;
};
export function workerChannel(worker: Worker): ManagedChannel {
  const messages = new Set<(message: ManagedMessage) => void>();
  const failures = new Set<(error: Error) => void>();
  const receive = (event: MessageEvent<unknown>) => {
    const message = event.data;
    if (isManagedMessage(message)) {
      for (const listener of messages) {
        listener(message);
      }
    }
  };
  const failed = (event: ErrorEvent) => {
    for (const listener of failures) {
      listener(new Error(event.message || "managed Worker failed"));
    }
  };
  const invalid = () => {
    for (const listener of failures) {
      listener(new Error("managed Worker message could not be decoded"));
    }
  };
  worker.addEventListener("message", receive);
  worker.addEventListener("error", failed);
  worker.addEventListener("messageerror", invalid);
  return {
    send: (message) => {
      // oxlint-disable-next-line unicorn/require-post-message-target-origin -- Dedicated Workers do not accept a Window targetOrigin.
      worker.postMessage(message);
    },
    onMessage(listener) {
      messages.add(listener);
      return () => {
        messages.delete(listener);
      };
    },
    onFailure(listener) {
      failures.add(listener);
      return () => {
        failures.delete(listener);
      };
    },
    // oxlint-disable-next-line typescript/require-await -- Uniform platform disposal returns a Promise; Worker termination is synchronous.
    async terminate() {
      worker.removeEventListener("message", receive);
      worker.removeEventListener("error", failed);
      worker.removeEventListener("messageerror", invalid);
      messages.clear();
      failures.clear();
      worker.terminate();
    },
  };
}
export function webWorkerDriver(options: WebWorkerDriverOptions): HostDriver {
  const base = typeof document === "undefined" ? globalThis.location.href : document.baseURI;
  const module = new URL(String(options.module), base).href;
  return {
    // oxlint-disable-next-line typescript/require-await -- Platform Worker construction is synchronous; the driver contract is asynchronous.
    async spawn() {
      const worker = new Worker(options.workerURL ?? new URL("worker.js", import.meta.url), {
        type: "module",
      });
      const channel = workerChannel(worker);
      return {
        ...channel,
        send(message) {
          channel.send(
            message.type === "init"
              ? {
                  ...message,
                  module,
                  ...(options.wasmURL === undefined
                    ? {}
                    : { wasmURL: new URL(String(options.wasmURL), base).href }),
                }
              : message,
          );
        },
      };
    },
  };
}
export function createHostManager(options: HostManagerOptions): HostManager {
  return createManagedHostManager(options);
}
