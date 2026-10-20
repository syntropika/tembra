import type { HostDriver, HostManager } from "@tembra/sdk/node";
import { check } from "@tembra/testing";

/** Drop only incoming opaque RPC frames; keep parent-to-child traffic live. Platform/process failure events remain observable and are counted. */
export function silentPeer(driver: HostDriver) {
  const observations = { spawns: 0, terminations: 0, platformFailures: 0, droppedFrames: 0 };
  let silence: (() => void) | undefined;
  return {
    observations,
    silence: () => {
      check(silence, "managed peer has been spawned");
      silence();
    },
    driver: {
      async spawn() {
        const channel = await driver.spawn();
        observations.spawns += 1;
        let silent = false;
        silence = () => {
          silent = true;
        };
        return {
          ...channel,
          onMessage(listener) {
            return channel.onMessage((message) => {
              if (silent && message.type === "frames") {
                observations.droppedFrames += 1;
                return;
              }
              listener(message);
            });
          },
          onFailure(listener) {
            // oxlint-disable-next-line promise/prefer-await-to-callbacks -- Count actual platform failure events without manufacturing one.
            return channel.onFailure((error) => {
              observations.platformFailures += 1;
              listener(error);
            });
          },
          async terminate() {
            await channel.terminate();
            observations.terminations += 1;
          },
        };
      },
    } satisfies HostDriver,
  };
}
export async function managedLiveness(
  manager: HostManager,
  controlled: ReturnType<typeof silentPeer>,
): Promise<void> {
  const first = await manager.start();
  check(
    first.status() === "ready" && first.failure() === undefined,
    "initial readiness has no failure",
  );
  const initial = first.activity();
  // No application references or calls exist. The next core ping proves the environment remains retained.
  // oxlint-disable-next-line promise/avoid-new -- Wait beyond the actual default core ping interval with no application demand.
  await new Promise<void>((resolve) => {
    setTimeout(resolve, 11_000);
  });
  check(
    first.activity().receivedFrames > initial.receivedFrames,
    "idle retained environment continues authoritative core liveness traffic",
  );
  check(
    first.status() === "ready",
    "zero application references do not stop a managed environment",
  );
  controlled.silence();
  let timeout: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      first.closed,
      // oxlint-disable-next-line promise/avoid-new -- This test deadline bounds the real 30-second core liveness policy.
      new Promise<never>((_resolve, reject) => {
        timeout = setTimeout(() => {
          reject(new Error("core liveness did not reclaim silent peer"));
        }, 45_000);
      }),
    ]);
  } finally {
    clearTimeout(timeout);
  }
  check(first.status() === "failed", "silent loss changes canonical manager status");
  const failure = first.failure();
  check(failure instanceof Error, "silent peer retains its terminal failure");
  check("code" in failure && failure.code === "unavailable", "preserves core unavailable code");
  check(
    "reason" in failure && failure.reason === "liveness",
    "core liveness owns silent-loss detection",
  );
  check(
    failure.cause !== null &&
      typeof failure.cause === "object" &&
      "reason" in failure.cause &&
      failure.cause.reason === "liveness",
    "preserves the original structured core cause",
  );
  check(
    controlled.observations.platformFailures === 0,
    "no platform crash or disconnect supplied the failure",
  );
  check(
    controlled.observations.droppedFrames > 0,
    "the real core attempted traffic across silent transport",
  );
  const initialTerminations = controlled.observations.terminations;
  check(initialTerminations === 1, "liveness reclaims the physical environment once");
  await manager.stop();
  check(manager.current === undefined, "stopping a failed instance clears ownership");
  check(first.failure() === failure, "cleanup preserves the first causal liveness failure");
  const second = await manager.start();
  check(
    second.generation === 2 && second.failure() === undefined,
    "a replacement has independent failure diagnostics",
  );
  await manager.stop();
  check(
    second.status() === "stopped" && second.failure() === undefined,
    "normal stop records no failure",
  );
  check(controlled.observations.terminations === 2, "replacement physical cleanup completes");
}
