import { orb } from "@syntropika/tembra-sdk";

const attempt = (run: () => unknown) => {
  try {
    return String(run());
  } catch (error) {
    return `denied: ${(error as Error).message}`;
  }
};

export default orb({
  name: "denied",
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
      },
    },
  },
});
