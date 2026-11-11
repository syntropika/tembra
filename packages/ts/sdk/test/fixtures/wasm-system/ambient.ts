import { orb } from "@syntropika/tembra-sdk";
export default orb({
  name: "ambient",
  apiVersion: 1,
  services: {
    policy: {
      methods: {
        // Without clock or random grants the runtime's internal clocks are frozen at the epoch
        // and its random seed is fixed: ambient builtins never observe host time or entropy.
        inspect() {
          return JSON.stringify({
            now: Date.now(),
            date: new Date().getTime(),
            call: Date(),
            constructor: new (new Date(0).constructor as DateConstructor)().getTime(),
            random: Math.random(),
            pure: new Date(0).toISOString(),
          });
        },
      },
    },
  },
});
