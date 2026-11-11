import { orb } from "@syntropika/tembra-sdk";

export default orb({
  name: "doubler",
  apiVersion: 1,
  services: {
    math: {
      methods: {
        double(_environment: unknown, _context: unknown, value: number) {
          return (value * 2) & 255;
        },
      },
    },
  },
});
