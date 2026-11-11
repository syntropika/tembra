import {
  orb,
  readChunks,
  type Context,
  type Declaration,
  type WitReference,
} from "@syntropika/tembra-sdk";

const FRAMES = "tembra-test:streams/frames@1.0.0";
const ALIASED = "tembra-test:streams/aliased@1.0.0";
/** An interface this contract does not import. */
const RELAYED = "tembra-test:streams/relayed@1.0.0";

/** Producer modes and native `ping` arguments; `source_stream_imports.rs` uses the same numbers. */
const CLEAN = 0;
const QUIET = 4;
const GATED = 6;
const PROBE = 1000;
const STARTED = 2000;
const CLOSED = 3000;
/** Read styles of `read`; `+ ALIAS` reads the aliased import instead. */
const BYTES = 1;
const PROBED = 2;
const ALIAS = 10;

type Frames = {
  frames(count: number, size: number, mode: number): Promise<AsyncIterable<Uint8Array>>;
  ping(value: number): number;
};

function create(self: Declaration) {
  return {
    frames: self.get<Frames>(FRAMES, ["main"]),
    aliased: self.get<Frames>(ALIASED, ["main"]),
  };
}
type Environment = ReturnType<typeof create>;

/** A reference and context `retain` kept beyond their callback. */
let kept: { reference: WitReference<Frames>; context: Context } | undefined;

/** Byte `j` of chunk `i` is `(i + j) mod 256`; count mismatches while advancing `at`. */
function check(chunk: Uint8Array, at: { index: number; offset: number }, size: number): number {
  let bad = 0;
  for (let k = 0; k < chunk.length; k++) {
    if (chunk[k] !== ((at.index + at.offset) & 255)) bad++;
    if (++at.offset === size) {
      at.offset = 0;
      at.index++;
    }
  }
  return bad;
}

function message(error: unknown): string {
  return error instanceof Error ? `${error.name}: ${error.message}` : "thrown " + String(error);
}

/** Drop a stream end now (the SDK `readChunks` release), even if it was never read. */
function drop(stream: AsyncIterable<Uint8Array>): void {
  (stream as unknown as { drop?(): void }).drop?.();
}

async function refused(run: () => unknown): Promise<string> {
  try {
    await run();
    return "accepted";
  } catch (error) {
    return message(error);
  }
}

export default orb({
  name: "reader",
  apiVersion: 1,
  services: {
    reader: {
      create,
      methods: {
        ping(_environment: Environment, _context: Context, value: number): number {
          return value + 1;
        },
        async read(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
          mode: number,
          take: bigint,
          style: number,
        ): Promise<[bigint, number, number]> {
          const source = style >= ALIAS ? environment.aliased : environment.frames;
          const how = style % ALIAS;
          const limit = take > BigInt(Number.MAX_SAFE_INTEGER) ? Infinity : Number(take);
          const stream = await source.frames(count, size, mode, context);
          const at = { index: 0, offset: 0 };
          let bytes = 0;
          let bad = 0;
          let reads = 0;
          if (how === BYTES) {
            // The lifted stream itself, without the helper, checked byte by byte.
            outer: for await (const chunk of stream) {
              reads++;
              for (const byte of chunk) {
                if (byte !== ((at.index + at.offset) & 255)) bad++;
                if (++at.offset === size) {
                  at.offset = 0;
                  at.index++;
                }
                if (++bytes >= limit) break outer;
              }
            }
          } else {
            for await (const chunk of readChunks(stream)) {
              reads++;
              bad += check(chunk, at, size);
              bytes += chunk.length;
              if (how === PROBED && reads === 1) source.ping(PROBE, context);
              if (bytes >= limit) break;
            }
          }
          return [BigInt(bytes), bad, reads];
        },
        async failure(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
          mode: number,
        ): Promise<[bigint, string]> {
          let stream: AsyncIterable<Uint8Array>;
          try {
            stream = await environment.frames.frames(count, size, mode, context);
          } catch (error) {
            return [0n, "import:" + message(error)];
          }
          const at = { index: 0, offset: 0 };
          let bytes = 0;
          let bad = 0;
          try {
            for await (const chunk of readChunks(stream)) {
              bad += check(chunk, at, size);
              bytes += chunk.length;
            }
          } catch (error) {
            return [BigInt(bytes), (bad ? "corrupt:" : "read:") + message(error)];
          }
          return [BigInt(bytes), bad ? "corrupt" : "end"];
        },
        async ignore(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
          mode: number,
        ): Promise<number> {
          await environment.frames.frames(count, size, mode, context);
          return 7;
        },
        async closing(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
        ): Promise<number> {
          const chunks = readChunks(await environment.frames.frames(count, size, QUIET, context));
          const first = await chunks.next();
          if (first.done || first.value.length !== size)
            throw new Error("the first chunk is missing");
          // Ends the iteration, which drops the stream end.
          await chunks.return(undefined);
          // Answered only once the producer saw its stream closed, so this callback is still
          // running when the close reaches it.
          return environment.frames.ping(CLOSED, context);
        },
        async many(
          environment: Environment,
          context: Context,
          n: number,
          count: number,
          size: number,
        ): Promise<bigint> {
          const streams = await Promise.all(
            Array.from({ length: n }, () => environment.frames.frames(count, size, CLEAN, context)),
          );
          let total = 0;
          for (const stream of streams)
            for await (const chunk of readChunks(stream)) total += chunk.length;
          return BigInt(total);
        },
        async abandon(
          environment: Environment,
          context: Context,
          count: number,
          size: number,
        ): Promise<number> {
          const pending = environment.frames.frames(count, size, GATED, context);
          pending.then(drop, () => {});
          // Answered once the gated call runs; the callback then returns with its import pending.
          return environment.frames.ping(STARTED, context);
        },
        async retain(_environment: Environment, context: Context): Promise<number> {
          const reference = await context.get<Frames>(FRAMES, ["main"]);
          kept = { reference, context };
          // Valid inside the root that acquired it.
          return reference.ping(1, context);
        },
        async forge(environment: Environment, context: Context, which: number): Promise<string> {
          switch (which) {
            case 0: {
              // Arguments outside the typed import's signature, then a valid call in the same
              // callback: the refused argument left no selection behind.
              const refusal = await refused(() => environment.frames.frames(-1, 1, CLEAN, context));
              return `${refusal} | ${environment.frames.ping(41, context)}`;
            }
            case 1: {
              // A reference released before its call.
              const reference = await context.get<Frames>(FRAMES, ["main"]);
              reference.release();
              return refused(() => reference.frames(1, 1, CLEAN, context));
            }
            case 2:
              // A reference whose root claim ended with an earlier callback's root.
              return refused(() => kept!.reference.frames(1, 1, CLEAN));
            case 3:
              // The context of an ended callback as the typed import's context.
              return refused(() => environment.frames.frames(1, 1, CLEAN, kept!.context));
            case 4:
              // A binding this contract does not import.
              return refused(() => context.get<Frames>(RELAYED, ["main"]));
            default:
              throw new Error("unknown forgery");
          }
        },
      },
    },
  },
});
