import { fork } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

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

export type ForkDriverOptions = {
  /** Trusted facet module loaded inside the child's own Rust Host. */
  module: string | URL;
  binary?: string | URL;
  /** Override only when distributing a custom child bootstrap. */
  bootstrap?: string | URL;
  execPath?: string;
};
function isMessage(value: unknown): value is ManagedMessage {
  return (
    value !== null && typeof value === "object" && "type" in value && typeof value.type === "string"
  );
}
function processChannel(child: ChildProcess): ManagedChannel {
  const messages = new Set<(message: ManagedMessage) => void>();
  const failures = new Set<(error: Error) => void>();
  const receive = (message: unknown) => {
    if (isMessage(message)) {
      for (const listener of messages) {
        listener(message);
      }
    }
  };
  const failure = (error: Error) => {
    for (const listener of failures) {
      listener(error);
    }
  };
  const exited = (code: number | null, signal: string | null) => {
    failure(new Error(`managed process exited (${code ?? signal})`));
  };
  const disconnected = () => {
    failure(new Error("managed process IPC disconnected"));
  };
  child.on("message", receive);
  child.on("error", failure);
  child.on("exit", exited);
  child.on("disconnect", disconnected);
  return {
    send(message) {
      if (!child.connected) {
        throw new Error("managed process IPC disconnected");
      }
      // oxlint-disable-next-line promise/prefer-await-to-callbacks -- Node IPC exposes transport admission failures through this callback.
      child.send(message, (error) => {
        if (error) {
          failure(error);
        }
      });
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
    async terminate() {
      child.off("message", receive);
      child.off("error", failure);
      child.off("exit", exited);
      child.off("disconnect", disconnected);
      messages.clear();
      failures.clear();
      if (child.connected) {
        child.disconnect();
      }
      if (child.exitCode !== null || child.signalCode !== null) {
        return;
      }
      // oxlint-disable-next-line promise/avoid-new -- Physical process exit is an EventEmitter event.
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          child.kill("SIGKILL");
        }, 1000);
        child.once("exit", () => {
          clearTimeout(timer);
          resolve();
        });
        child.kill("SIGTERM");
      });
    },
  };
}
export function forkDriver(options: ForkDriverOptions): HostDriver {
  let module: string;
  if (options.module instanceof URL) {
    module = options.module.href;
  } else if (options.module.startsWith("file:")) {
    module = new URL(options.module).href;
  } else {
    module = pathToFileURL(path.resolve(options.module)).href;
  }
  const binary = options.binary instanceof URL ? fileURLToPath(options.binary) : options.binary;
  return {
    // oxlint-disable-next-line typescript/require-await -- fork construction is synchronous; readiness is a separate managed handshake.
    async spawn() {
      const bootstrap = options.bootstrap ?? new URL("child.js", import.meta.url);
      const child = fork(
        bootstrap instanceof URL ? fileURLToPath(bootstrap) : bootstrap,
        [
          JSON.stringify({
            module,
            ...(binary === undefined ? {} : { binary }),
          }),
        ],
        {
          stdio: ["ignore", "inherit", "inherit", "ipc"],
          execArgv: [],
          ...(options.execPath === undefined ? {} : { execPath: options.execPath }),
        },
      );
      return processChannel(child);
    },
  };
}
export function createHostManager(options: HostManagerOptions): HostManager {
  return createManagedHostManager(options);
}
