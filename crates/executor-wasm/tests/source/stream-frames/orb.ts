import { orb, writeChunks, type Context, type Declaration } from "@syntropika/tembra-sdk";

const LEDGER = "tembra-test:source-streams/ledger@1.0.0";

/** Producer modes; `source_streams.rs` uses the same numbers. */
const CHUNKS = 0;
const BYTES = 1;
const ARRAY = 2;
const FAIL_AFTER_HEAD = 3;
const FAIL_BEFORE_HEAD = 4;
const NOT_A_STREAM = 5;
const IDLE = 6;
const DEPENDENCY = 7;
const THROW_UNDEFINED = 8;
const CLEANUP = 9;
const GATED = 10;
const NUMBERS = 11;
const RAW_ARRAY = 12;
const IDLE_HANDLED = 13;

/**
 * Byte `j` of chunk `i` is `(i + j) mod 256`: a view of one pattern built while the module
 * evaluates, so producing a chunk spends no per-byte work of the callback's fuel.
 */
const PATTERN = new Uint8Array((1 << 16) + 256);
for (let offset = 0; offset < 256; offset++) PATTERN[offset] = offset;
for (let length = 256; length < PATTERN.length; length *= 2) PATTERN.copyWithin(length, 0, length);
function chunk(index: number, size: number): Uint8Array {
  const start = index & 255;
  return PATTERN.subarray(start, start + size);
}

function create(self: Declaration) {
  return {
    ledger: self.get<{ note(event: string): Promise<string> }>(LEDGER, ["main"]),
  };
}
type Environment = ReturnType<typeof create>;

/** A stream result outside the SDK's `AsyncIterable<Uint8Array>` type. */
function unchecked(value: unknown): AsyncIterable<Uint8Array> {
  return value as AsyncIterable<Uint8Array>;
}

export default orb({
  name: "frames",
  apiVersion: 1,
  services: {
    frames: {
      create,
      methods: {
        ping(_environment: Environment, _context: Context, value: number): number {
          return value + 1;
        },
        async frames(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
          mode: number,
        ): Promise<AsyncIterable<Uint8Array>> {
          switch (mode) {
            case CHUNKS:
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
            case BYTES:
              // One-byte chunks: the finest granularity a byte stream accepts.
              return writeChunks(
                (async function* () {
                  for (let index = 0; index < count; index++)
                    for (let offset = 0; offset < size; offset++)
                      yield Uint8Array.of((index + offset) & 255);
                })(),
              );
            case ARRAY: {
              // A whole array as the only chunk of a synchronous iterable.
              const bytes = new Uint8Array(count * size);
              for (let index = 0; index < count; index++)
                bytes.set(chunk(index, size), index * size);
              return writeChunks([bytes]);
            }
            case FAIL_AFTER_HEAD:
              return writeChunks(
                (async function* () {
                  for (let index = 0; index < count; index++) {
                    if (index === count >> 1) throw new Error("producer failed after its head");
                    yield chunk(index, size);
                  }
                })(),
              );
            case FAIL_BEFORE_HEAD:
              throw new Error("producer refused before its head");
            case NOT_A_STREAM:
              return unchecked(42);
            case IDLE:
              return writeChunks(
                (async function* () {
                  try {
                    yield chunk(0, size);
                    console.log("producer:idle");
                    // Idle until the reader is gone or the callback ends.
                    for (;;) await tembra.wait(5000);
                  } finally {
                    console.log("producer:returned");
                  }
                })(),
              );
            case IDLE_HANDLED:
              // Like IDLE, but the author handles the host revoking its pending wait (the reader
              // was dropped or the root cancelled) and ends the source normally.
              return writeChunks(
                (async function* () {
                  yield chunk(0, size);
                  console.log("producer:idle");
                  try {
                    for (;;) await tembra.wait(5000);
                  } catch (error) {
                    console.log(
                      "producer:revoked:" +
                        (error instanceof Error ? error.message : String(error)),
                    );
                  }
                })(),
              );
            case DEPENDENCY:
              return writeChunks(
                (async function* () {
                  yield chunk(0, size);
                  const seen = await environment.ledger.note("after-head", context);
                  console.log("ledger:" + seen);
                  for (let index = 1; index < count; index++) yield chunk(index, size);
                })(),
              );
            case THROW_UNDEFINED:
              return writeChunks(
                (async function* () {
                  for (let index = 0; index < count; index++) yield chunk(index, size);
                  throw undefined;
                })(),
              );
            case CLEANUP:
              return writeChunks(
                (async function* () {
                  try {
                    for (let index = 0; ; index++) yield chunk(index, size);
                  } finally {
                    const seen = await environment.ledger.note("cleanup", context);
                    console.log("cleanup:" + seen);
                  }
                })(),
              );
            case GATED:
              return writeChunks(
                (async function* () {
                  yield chunk(0, size);
                  const seen = await environment.ledger.note("gated", context);
                  console.log("gated:" + seen);
                  for (let index = 1; ; index++) yield chunk(index, size);
                })(),
              );
            case NUMBERS:
              // A per-byte `AsyncIterable<number>`: the runtime writes each number as one byte.
              return unchecked(
                (async function* () {
                  for (let index = 0; index < count; index++)
                    for (let offset = 0; offset < size; offset++) yield (index + offset) & 255;
                })(),
              );
            case RAW_ARRAY: {
              // A bare `Uint8Array` result: an iterable of numbers, each written as one byte.
              const bytes = new Uint8Array(count * size);
              for (let index = 0; index < count; index++)
                bytes.set(chunk(index, size), index * size);
              return unchecked(bytes);
            }
            default:
              throw new Error("unknown mode");
          }
        },
      },
    },
  },
});
