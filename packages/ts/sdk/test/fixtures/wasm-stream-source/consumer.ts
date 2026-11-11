import {
  orb,
  readChunks,
  writeChunks,
  type Context,
  type Declaration,
  type WitMethods,
} from "@syntropika/tembra-sdk";
import type { TembraTestTypedStreamFrames as Frames, relay } from "./contract.wit";

function create(self: Declaration) {
  return { frames: self.get<typeof Frames>("tembra-test:typed-stream/frames", ["main"]) };
}
type Environment = ReturnType<typeof create>;

export default orb({
  name: "typed-stream-consumer",
  apiVersion: 1,
  services: {
    relay: {
      create,
      methods: {
        async summarize(environment: Environment, context: Context, count: number, size: number) {
          const stream = await environment.frames.frames(count, size, context);
          let bytes = 0;
          let checksum = 0;
          for await (const chunk of readChunks(stream)) {
            bytes += chunk.length;
            for (const byte of chunk) checksum += byte;
          }
          return { bytes: BigInt(bytes), checksum: BigInt(checksum) };
        },
        async bytewise(environment: Environment, context: Context, count: number, size: number) {
          const stream = await environment.frames.frames(count, size, context);
          let bytes = 0;
          let checksum = 0;
          for await (const byte of stream) {
            bytes++;
            checksum += byte;
          }
          return { bytes: BigInt(bytes), checksum: BigInt(checksum) };
        },
        async forward(environment: Environment, context: Context, count: number, size: number) {
          return writeChunks(readChunks(await environment.frames.frames(count, size, context)));
        },
      } satisfies WitMethods<typeof relay, Environment>,
    },
  },
});
