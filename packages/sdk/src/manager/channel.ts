import { ownManagerLink } from "./bridge.js";
import type { RpcEndpoint } from "./bridge.js";

export type ManagedMessage = { type: string; [key: string]: unknown };
export function isManagedMessage(value: unknown): value is ManagedMessage {
  return (
    value !== null && typeof value === "object" && "type" in value && typeof value.type === "string"
  );
}
/** Platform drivers carry opaque Rust RPC frames and bounded lifecycle control messages. */
export type ManagedChannel = {
  send: (message: ManagedMessage) => void;
  onMessage: (listener: (message: ManagedMessage) => void) => () => void;
  onFailure: (listener: (error: Error) => void) => () => void;
  terminate: () => Promise<void>;
};
export type HostDriver = {
  spawn: () => Promise<ManagedChannel>;
};
export function deferred<T>() {
  let fulfill!: (value: T | PromiseLike<T>) => void;
  let fail!: (reason: unknown) => void;
  // oxlint-disable-next-line promise/avoid-new -- Lifecycle events settle one explicitly owned observer.
  const promise = new Promise<T>((resolve, reject) => {
    fulfill = resolve;
    fail = reject;
  });
  return { promise, resolve: fulfill, reject: fail };
}
export async function deadline<T>(
  promise: Promise<T>,
  milliseconds: number,
  reason: string,
): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      promise,
      // oxlint-disable-next-line promise/avoid-new -- The deadline owns a cancellable platform timer.
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => {
          reject(Object.assign(new Error(reason), { code: "unavailable", reason }));
        }, milliseconds);
      }),
    ]);
  } finally {
    if (timer !== undefined) {
      clearTimeout(timer);
    }
  }
}

/** At most one detached outbound batch awaits acknowledgment; Rust bounds each queued direction. */
export function framePump(
  endpoint: RpcEndpoint,
  channel: ManagedChannel,
  failed: (error: Error) => void,
  initiallyPaused = false,
  host?: object,
) {
  let ended = false;
  let released = false;
  let pending = false;
  let paused = initiallyPaused;
  const activity = { sentFrames: 0, receivedFrames: 0, lastMessageAt: Date.now() };
  const remove = channel.onMessage((message) => {
    try {
      activity.lastMessageAt = Date.now();
      if (message.type === "ack") {
        pending = false;
      }
      if (message.type !== "frames") {
        return;
      }
      if (!Array.isArray(message.frames) || message.frames.length > 256) {
        throw new Error("invalid IPC frame batch");
      }
      for (const frame of message.frames) {
        if (typeof frame !== "string") {
          throw new TypeError("invalid IPC frame");
        }
        endpoint.receive(frame);
        activity.receivedFrames += 1;
      }
      channel.send({ type: "ack" });
    } catch (error) {
      failed(error instanceof Error ? error : new Error(String(error)));
    }
  });
  const timer = setInterval(() => {
    if (ended || pending || paused) {
      return;
    }
    try {
      const frames = endpoint.drain();
      if (frames.length) {
        pending = true;
        activity.sentFrames += frames.length;
        channel.send({ type: "frames", frames });
      }
    } catch (error) {
      failed(error instanceof Error ? error : new Error(String(error)));
    }
  }, 2);
  let releaseOwnership: (() => void) | undefined;
  function detach() {
    if (ended) {
      return;
    }
    ended = true;
    clearInterval(timer);
    remove();
    releaseOwnership?.();
  }
  function release(close: boolean) {
    if (released) {
      return;
    }
    released = true;
    detach();
    try {
      if (close) {
        endpoint.close();
      }
    } finally {
      endpoint.free();
    }
  }
  const pump = {
    activity: () => Object.freeze({ ...activity }),
    start() {
      paused = false;
    },
    close() {
      release(true);
    },
  };
  if (host !== undefined) {
    releaseOwnership = ownManagerLink(host, () => {
      // Core has closed the connection and may already have dropped its native handle.
      // Release the issued wrapper before the platform disposes its remaining bindings.
      release(false);
    });
  }
  return pump;
}
