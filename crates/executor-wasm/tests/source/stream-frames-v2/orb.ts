import { orb, writeChunks, type Context } from "@syntropika/tembra-sdk";

/** The reload replacement of `stream-frames`: same contract, revision 2. */
export default orb({
  name: "frames",
  apiVersion: 1,
  services: {
    frames: {
      methods: {
        ping(_environment: unknown, _context: Context, value: number): number {
          return value + 2;
        },
        async frames(
          _environment: unknown,
          _context: Context,
          count: number,
          size: number,
        ): Promise<AsyncIterable<Uint8Array>> {
          return writeChunks(
            (function* () {
              for (let index = 0; index < count; index++) {
                const bytes = new Uint8Array(size);
                for (let offset = 0; offset < size; offset++)
                  bytes[offset] = (index + offset) & 255;
                yield bytes;
              }
            })(),
          );
        },
      },
    },
  },
});
