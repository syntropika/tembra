import { orb } from "@syntropika/tembra-sdk";

/** Imports a refused stream shape; installation must fail before this Orb ever runs. */
export default orb({
  name: "orb",
  apiVersion: 1,
  services: {
    probe: {
      methods: {
        ping(): number {
          return 1;
        },
      },
    },
  },
});
