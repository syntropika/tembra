import { orb } from "@syntropika/tembra-sdk";

// Each call waits before doubling, so its result arrives after the caller suspended.
export default orb({
  name: "delayed",
  apiVersion: 1,
  services: {
    delayed: {
      methods: {
        async double(_environment: unknown, _context: unknown, value: number) {
          await tembra.wait(40);
          return value * 2;
        },
      },
    },
  },
});
