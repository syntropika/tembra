import { orb, writeChunks, type Context } from "@syntropika/tembra-sdk";

export default orb({
  name: "failing",
  apiVersion: 1,
  services: {
    frames: {
      methods: {
        async frames(_environment: unknown, _context: Context, count: number, mode: string) {
          return writeChunks(
            (async function* () {
              for (let index = 0; index < count; index++) yield new Uint8Array(4096).fill(index);
              if (mode === "throw") throw new Error("producer failed");
              if (mode === "reject") await Promise.reject(new Error("producer rejected"));
            })(),
          );
        },
        async explode(): Promise<number> {
          throw new Error("uncaught author error");
        },
        async refuse(): Promise<number> {
          throw new Error("business refusal");
        },
      },
    },
  },
});
