import type { TembraHost } from "../host/index.js";
import { managerBridge, ownManager, checkManagedHostOpen } from "./bridge.js";
import type { RpcEndpoint } from "./bridge.js";
import { framePump, deadline, deferred } from "./channel.js";
import type { HostDriver, ManagedChannel } from "./channel.js";
import { connectionFailure, lifecycleFailure } from "./failure.js";

export type { HostDriver, ManagedChannel, ManagedMessage } from "./channel.js";

export type HostManagerOptions = {
  /** Parent shutdown stops owned children first, preserving parent services for child closers.
   * Run cleanup that needs child services before closing the parent. Started environments stay
   * retained until stop/replace/parent close, even with no application references. */
  host: TembraHost;
  name: string;
  driver: HostDriver;
  /** Finite startup/cleanup deadlines. Core RPC separately owns connection liveness. */
  startupTimeout?: number;
  stopTimeout?: number;
};
export type HostInstance = {
  readonly name: string;
  readonly generation: number;
  /** Readiness followed by observed platform/core liveness; no independent heartbeat runs here. */
  readonly status: () => "starting" | "ready" | "stopping" | "stopped" | "failed";
  /** First causal terminal failure, preserving core code/reason/cause. Normal stop leaves it undefined. */
  readonly failure: () => Error | undefined;
  /** Transport traffic, including core pings and control frames; not object demand or a health guarantee. */
  readonly activity: () => Readonly<{
    sentFrames: number;
    receivedFrames: number;
    lastMessageAt: number;
  }>;
  readonly closed: Promise<void>;
};
export type HostManager = {
  start: () => Promise<HostInstance>;
  stop: () => Promise<void>;
  replace: () => Promise<HostInstance>;
  readonly current: HostInstance | undefined;
};
/** Supervises execution environments. Replacing one never claims to preserve its in-memory state. */
export function createManagedHostManager(options: HostManagerOptions): HostManager {
  const parent = managerBridge(options.host);
  const startupTimeout = options.startupTimeout ?? 15_000;
  const stopTimeout = options.stopTimeout ?? 5000;
  if (
    !options.name ||
    options.name === parent.name ||
    !Number.isFinite(startupTimeout) ||
    startupTimeout <= 0 ||
    !Number.isFinite(stopTimeout) ||
    stopTimeout <= 0
  ) {
    throw new Error("invalid HostManager options");
  }
  let current: HostInstance | undefined;
  let starting: Promise<HostInstance> | undefined;
  let stopping: Promise<void> | undefined;
  let generation = 0;
  let shutdown: (() => Promise<void>) | undefined;
  const manager: HostManager = {
    get current() {
      return current;
    },
    async start() {
      checkManagedHostOpen(options.host);
      if (stopping) {
        await stopping;
        return await manager.start();
      }
      if (starting) {
        return await starting;
      }
      if (current?.status() === "ready") {
        return current;
      }
      starting = (async () => {
        if (current?.status() === "failed") {
          await current.closed;
        }
        const startedAt = Date.now();
        const spawned = options.driver.spawn();
        let channel: ManagedChannel;
        try {
          channel = await deadline(spawned, startupTimeout, "host_start_timeout");
        } catch (error) {
          // The platform may complete construction after the caller's deadline; retain cleanup ownership.
          void spawned
            .then(async (late) => {
              await late.terminate();
            })
            .catch(() => {
              /* A failed constructor owns no channel. */
            });
          throw error;
        }
        const ready = deferred<undefined>();
        const closed = deferred<undefined>();
        const stopped = deferred<undefined>();
        let cleaned = false;
        let status: ReturnType<HostInstance["status"]> = "starting";
        let failure: Error | undefined;
        let endpoint: RpcEndpoint;
        try {
          endpoint = parent.attach(options.name, true);
        } catch (error) {
          await channel.terminate();
          throw error;
        }
        const pump = framePump(endpoint, channel, failed, true, options.host);
        const cleanups: (() => void)[] = [];
        let cleaning: Promise<void> | undefined;
        async function cleanup() {
          cleaning ??= (async () => {
            cleaned = true;
            for (const remove of cleanups) {
              remove();
            }
            try {
              try {
                pump.close();
              } finally {
                await channel.terminate();
              }
            } finally {
              closed.resolve(undefined);
            }
          })();
          await cleaning;
        }
        function failed(error: Error) {
          if (status === "stopped") {
            return;
          }
          failure ??= error;
          if (status === "stopping") {
            stopped.reject(error);
            return;
          }
          status = "failed";
          ready.reject(error);
          // oxlint-disable-next-line promise/no-promise-in-callback -- Failure events must observe asynchronous platform cleanup.
          void cleanup().catch(() => {
            closed.resolve(undefined);
          });
        }

        cleanups.push(
          channel.onFailure(failed),
          channel.onMessage((message) => {
            if (message.type === "transport-ready") {
              pump.start();
            }
            if (message.type === "ready") {
              ready.resolve(undefined);
            }
            if (message.type === "stopped") {
              stopped.resolve(undefined);
            }
            if (message.type === "failure") {
              failed(connectionFailure(message));
            }
          }),
        );
        void endpoint.closed().then((reason) => {
          if (status !== "stopping") {
            failed(connectionFailure(reason));
          }
        }, failed);
        generation += 1;
        const instance: HostInstance = {
          name: options.name,
          generation,
          status: () => status,
          failure: () => failure,
          activity: pump.activity,
          closed: closed.promise,
        };
        shutdown = async () => {
          if (status === "stopped" || cleaned) {
            await closed.promise;
            return;
          }
          status = "stopping";
          let refusal: Error | undefined;
          try {
            channel.send({ type: "stop" });
            await deadline(stopped.promise, stopTimeout, "host_stop_timeout");
          } catch (error) {
            refusal = lifecycleFailure(error);
            failure ??= refusal;
          }
          status = "stopped";
          try {
            await cleanup();
          } catch (error) {
            refusal ??= lifecycleFailure(error);
            failure ??= refusal;
          }
          if (refusal) {
            throw refusal;
          }
        };
        try {
          channel.send({ type: "init", name: options.name, parent: parent.name });
          await deadline(
            Promise.all([ready.promise, endpoint.ready()]),
            Math.max(1, startupTimeout - (Date.now() - startedAt)),
            "host_start_timeout",
          );
          status = "ready";
          current = instance;
          return instance;
        } catch (error) {
          status = "failed";
          await cleanup();
          throw error;
        }
      })().finally(() => {
        starting = undefined;
      });
      return await starting;
    },
    async stop() {
      stopping ??= (async () => {
        if (starting) {
          try {
            await starting;
          } catch {
            /* Failed startup already owns cleanup. */
          }
        }
        try {
          await shutdown?.();
        } finally {
          current = undefined;
        }
      })().finally(() => {
        stopping = undefined;
      });
      await stopping;
    },
    async replace() {
      await manager.stop();
      return await manager.start();
    },
  };
  ownManager(options.host, manager.stop);
  return manager;
}
