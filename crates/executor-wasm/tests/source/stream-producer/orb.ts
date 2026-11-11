import { orb, writeChunks, type Context } from "@syntropika/tembra-sdk";

/** Producer modes; `source_stream_imports.rs` uses the same numbers. */
const CLEAN = 0;
const FAIL_AFTER = 1;
const QUIET = 4;
const FAIL_BEFORE = 5;
const THROW_UNDEFINED = 7;

const PATTERN = new Uint8Array((1 << 16) + 256);
for (let offset = 0; offset < 256; offset++) PATTERN[offset] = offset;
for (let length = 256; length < PATTERN.length; length *= 2) PATTERN.copyWithin(length, 0, length);
function chunk(index: number, size: number): Uint8Array {
  const start = index & 255;
  return PATTERN.subarray(start, start + size);
}

export default orb({
  name: "frames",
  apiVersion: 1,
  services: {
    frames: {
      methods: {
        ping(_environment: unknown, _context: Context, value: number): number {
          return value + 1;
        },
        async frames(
          _environment: unknown,
          _context: Context,
          count: number,
          size: number,
          mode: number,
        ): Promise<AsyncIterable<Uint8Array>> {
          switch (mode) {
            case CLEAN:
              return writeChunks(
                (async function* () {
                  let index = 0;
                  try {
                    for (; index < count; index++) yield chunk(index, size);
                  } finally {
                    console.log("producer:returned:" + index);
                  }
                })(),
              );
            case FAIL_AFTER:
              return writeChunks(
                (async function* () {
                  for (let index = 0; index < count; index++) {
                    if (index === count >> 1)
                      throw new Error("source producer failed after its head");
                    yield chunk(index, size);
                  }
                })(),
              );
            case QUIET:
              return writeChunks(
                (async function* () {
                  try {
                    yield chunk(0, size);
                    console.log("producer:quiet");
                    // The host revokes this wait once the reader is gone or the root is
                    // cancelled; the source then ends normally (`source_streams` covers the
                    // unhandled revocation, which traps).
                    try {
                      for (;;) await tembra.wait(5000);
                    } catch {
                      console.log("producer:revoked");
                    }
                  } finally {
                    console.log("producer:returned");
                  }
                })(),
              );
            case FAIL_BEFORE:
              throw new Error("source producer refused before its head");
            case THROW_UNDEFINED:
              return writeChunks(
                (async function* () {
                  for (let index = 0; index < count; index++) yield chunk(index, size);
                  throw undefined;
                })(),
              );
            default:
              throw new Error("unknown mode");
          }
        },
      },
    },
  },
});
