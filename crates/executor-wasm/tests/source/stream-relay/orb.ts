import {
  orb,
  readChunks,
  writeChunks,
  type Context,
  type Declaration,
} from "@syntropika/tembra-sdk";

const FRAMES = "tembra-test:streams/frames@1.0.0";

type Frames = {
  frames(count: number, size: number, mode: number): Promise<AsyncIterable<Uint8Array>>;
};

function create(self: Declaration) {
  return { frames: self.get<Frames>(FRAMES, ["main"]) };
}
type Environment = ReturnType<typeof create>;

/** Forwards the imported stream as its own whole result, directly or chunk by chunk. */
export default orb({
  name: "relay",
  apiVersion: 1,
  services: {
    relayed: {
      create,
      methods: {
        async frames(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
          mode: number,
        ): Promise<AsyncIterable<Uint8Array>> {
          return await environment.frames.frames(count, size, mode, context);
        },
        async chunked(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
          mode: number,
        ): Promise<AsyncIterable<Uint8Array>> {
          return writeChunks(
            readChunks(await environment.frames.frames(count, size, mode, context)),
          );
        },
      },
    },
  },
});
