import type { TembraHost } from "../host/index.js";
import { managerBridge } from "./bridge.js";
import { framePump, deadline } from "./channel.js";
import type { ManagedChannel } from "./channel.js";
import { connectionFailure, failureMessage } from "./failure.js";

/** Child-side adapter shared by process and Worker bootstraps. */
export function serveManagedHost(
  channel: ManagedChannel,
  create: (name: string) => Promise<TembraHost>,
  initialize: (host: TembraHost) => Promise<void>,
): void {
  let host: TembraHost | undefined;
  let pump: ReturnType<typeof framePump> | undefined;
  let initialized = false;
  let closing = false;
  const removeFailure = channel.onFailure(failed);
  const remove = channel.onMessage((message) => {
    if (message.type === "init" && !initialized) {
      initialized = true;
      void (async () => {
        if (typeof message.name !== "string" || typeof message.parent !== "string") {
          throw new TypeError("invalid Host initialization");
        }
        host = await create(message.name);
        const endpoint = managerBridge(host).attach(message.parent, false);
        if (closing) {
          endpoint.close();
          endpoint.free();
          await host.close();
          return;
        }
        pump = framePump(endpoint, channel, failed, false, host);
        void endpoint.closed().then((reason) => {
          if (!closing) {
            failed(connectionFailure(reason));
          }
        }, failed);
        channel.send({ type: "transport-ready" });
        await endpoint.ready();
        await initialize(host);
        channel.send({ type: "ready" });
      })().catch(failed);
    }
    if (message.type === "stop" && !closing) {
      closing = true;
      void (async () => {
        if (host) {
          await deadline(host.close(), 5000, "child_close_timeout");
        }
        pump?.close();
        // Retain the lifecycle listener until parent-owned termination. In Node it keeps IPC
        // referenced while the stopped acknowledgment and the parent's final writes settle.
        channel.send({ type: "stopped" });
      })().catch(stopFailed);
    }
  });
  async function stopFailed(error: unknown) {
    remove();
    removeFailure();
    try {
      channel.send(failureMessage(error));
    } catch {
      try {
        await channel.terminate();
      } catch {
        /* The parent transport and child environment are already unavailable. */
      }
    }
  }
  function failed(error: unknown) {
    if (closing) {
      return;
    }
    closing = true;
    try {
      channel.send(failureMessage(error));
    } catch {
      /* Parent may already be gone. */
    }
    pump?.close();
    remove();
    removeFailure();
    // oxlint-disable-next-line promise/no-promise-in-callback -- Fault events retain a finite cleanup deadline before destroying the environment.
    void (async () => {
      try {
        if (host) {
          await deadline(host.close(), 5000, "child_close_timeout");
        }
      } finally {
        await channel.terminate();
      }
    })().catch(() => {
      /* The failed transport cannot carry another cleanup result. */
    });
  }
}
