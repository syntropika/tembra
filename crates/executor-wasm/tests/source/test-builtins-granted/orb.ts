import { orb } from "@syntropika/tembra-sdk";
import { now } from "wasi:clocks/system-clock@0.3.0";
import { getInsecureSeed } from "wasi:random/insecure-seed@0.3.0";

const attempt = (run: () => unknown) => {
  try {
    return String(run());
  } catch (error) {
    return `denied: ${(error as Error).message}`;
  }
};

export default orb({
  name: "granted",
  apiVersion: 1,
  services: {
    checks: {
      methods: {
        probe() {
          return JSON.stringify({
            call: attempt(() => Date()),
            construct: attempt(() => new Date().getTime()),
            now: attempt(() => Date.now()),
            constructor: attempt(() => (new Date(0).constructor as DateConstructor)()),
            random: attempt(() => Math.random()),
            pure: `${new Date(Date.UTC(2020, 1, 29)).getUTCDate()}/${Date.parse("1970-01-02T00:00:00Z")}`,
          });
        },
        wall() {
          const instant = now();
          return `${instant.seconds},${instant.nanoseconds}`;
        },
        seed() {
          return getInsecureSeed().map(String).join(",");
        },
      },
    },
  },
});
