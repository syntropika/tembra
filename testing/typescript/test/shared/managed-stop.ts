import type { HostDriver } from "@tembra/sdk/browser";

/** Lose or reject one graceful-stop acknowledgment while retaining real platform cleanup. */
export function refuseFirstStop(driver: HostDriver, outcome: "timeout" | "failure") {
  const activity = { acknowledgments: 0, terminations: 0, spawns: 0 };
  return {
    activity,
    driver: {
      async spawn() {
        const channel = await driver.spawn();
        activity.spawns += 1;
        const refuse = activity.spawns === 1;
        return {
          ...channel,
          onMessage(listener) {
            return channel.onMessage((message) => {
              if (refuse && message.type === "stopped") {
                activity.acknowledgments += 1;
                if (outcome === "failure") {
                  listener({ type: "failure", message: "stop acknowledgment rejected" });
                }
                return;
              }
              listener(message);
            });
          },
          async terminate() {
            await channel.terminate();
            activity.terminations += 1;
          },
        };
      },
    } satisfies HostDriver,
  };
}
