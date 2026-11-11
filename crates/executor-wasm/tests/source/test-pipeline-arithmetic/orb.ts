import { orb } from "@syntropika/tembra-sdk";

// A synchronous WIT function is implemented synchronously.
export default orb({
  name: "arithmetic",
  apiVersion: 1,
  services: {
    arithmetic: {
      methods: {
        double(_environment: unknown, _context: unknown, value: number) {
          return value * 2;
        },
      },
    },
  },
});
