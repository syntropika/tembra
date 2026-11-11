import { orb, writeChunks, type Context, type WitMethods } from "@syntropika/tembra-sdk";
import type { frames } from "./contract.wit";
export default orb({
  name: "typed-stream",
  apiVersion: 1,
  services: {
    frames: {
      methods: {
        async frames(_environment: unknown, _context: Context, count: number, size: number) {
          return writeChunks(
            (async function* () {
              for (let index = 0; index < count; index++) {
                const value: number = (index * 17 + 23) & 255;
                yield new Uint8Array(size).fill(value);
              }
            })(),
          );
        },
      } satisfies WitMethods<typeof frames, unknown>,
    },
  },
});
