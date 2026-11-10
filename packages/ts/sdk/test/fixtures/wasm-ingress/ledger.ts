import { orb, type Context } from "@syntropika/tembra-sdk";

const entries: string[] = [];

export default orb({
  name: "ledger",
  apiVersion: 1,
  services: {
    ledger: {
      methods: {
        note(_environment: unknown, _context: Context, entry: string) {
          entries.push(entry);
          return entries.length;
        },
        entries() {
          return [...entries];
        },
      },
    },
  },
});
