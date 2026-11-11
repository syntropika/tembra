import { orb, type Context } from "@syntropika/tembra-sdk";

/** The reload replacement of `stream-reader`: same contract, revision 2, serving `ping` only. */
export default orb({
  name: "reader",
  apiVersion: 1,
  services: {
    reader: {
      methods: {
        ping(_environment: unknown, _context: Context, value: number): number {
          return value + 2;
        },
        async read(): Promise<[bigint, number, number]> {
          throw new Error("revision 2 does not read");
        },
        async failure(): Promise<[bigint, string]> {
          throw new Error("revision 2 does not read");
        },
        async ignore(): Promise<number> {
          throw new Error("revision 2 does not read");
        },
        async closing(): Promise<number> {
          throw new Error("revision 2 does not read");
        },
        async many(): Promise<bigint> {
          throw new Error("revision 2 does not read");
        },
        async abandon(): Promise<number> {
          throw new Error("revision 2 does not read");
        },
        async retain(): Promise<number> {
          throw new Error("revision 2 does not read");
        },
        async forge(): Promise<string> {
          throw new Error("revision 2 does not read");
        },
      },
    },
  },
});
