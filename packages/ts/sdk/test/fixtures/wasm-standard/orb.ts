import { orb, readChunks } from "@syntropika/tembra-sdk";
import { getRandomBytes, getRandomU64 } from "wasi:random/random@0.3.0";
import { now, waitFor } from "wasi:clocks/monotonic-clock@0.3.0";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import type { Descriptor } from "wasi:filesystem/types@0.3.0";

/** Owned by this revision across callbacks until the revision retires. */
let retained: Descriptor | undefined;

/** The single preopened mount granted to this Orb. */
function root(): [Descriptor, string] {
  const [preopen] = getDirectories();
  if (!preopen) throw new Error("no preopened mount");
  return preopen;
}

/** Read a file from `offset` to its end and require a successful completion. */
async function read(file: Descriptor, offset: bigint): Promise<Uint8Array> {
  const [stream, done] = file.readViaStream(offset);
  const chunks: Uint8Array[] = [];
  for await (const chunk of readChunks(stream)) chunks.push(chunk);
  const result = await done.read();
  if (result.tag !== "ok") throw new Error(`read failed: ${result.val.tag}`);
  const bytes = new Uint8Array(chunks.reduce((total, chunk) => total + chunk.length, 0));
  let position = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, position);
    position += chunk.length;
  }
  return bytes;
}

/** Return the standard `error-code` of a refused call. */
async function refusal(operation: Promise<unknown>): Promise<string> {
  try {
    await operation;
    return "accepted";
  } catch (error) {
    return (error as { payload?: { tag?: string } }).payload?.tag ?? String(error);
  }
}

export default orb({
  name: "standard",
  apiVersion: 1,
  services: {
    scenario: {
      methods: {
        async probe() {
          const random = getRandomBytes(16n);
          const word = getRandomU64();
          const before = now();
          await waitFor(5_000_000n);
          const waited = now() - before;
          const [directory, mount] = root();
          const file = await directory.openAt({}, "bytes.bin", {}, { read: true });
          const stat = await file.stat();
          const bytes = await read(file, 0n);
          const beyond = await read(file, (1n << 53n) + 17n);
          retained = file;
          return JSON.stringify({
            randomLength: random.length,
            randomIsBytes: random instanceof Uint8Array,
            wordIsBigInt: typeof word === "bigint",
            waitedAtLeast5ms: waited >= 5_000_000n,
            mount,
            type: stat.type.tag,
            size: stat.size.toString(),
            length: bytes.length,
            pattern: bytes.every((value, index) => value === index % 256),
            beyond: beyond.length,
          });
        },
        async reuse() {
          if (!retained) throw new Error("no retained descriptor");
          const stat = await retained.stat();
          const tail = await read(retained, stat.size - 4n);
          return JSON.stringify({ size: stat.size.toString(), tail: Array.from(tail) });
        },
        async hang() {
          // Waits until the caller cancels; cancellation must not revoke the retained file. The
          // marker lets a caller cancel only after the wait was really entered.
          console.info("standard:waiting");
          await waitFor(60_000_000_000n);
          return "elapsed";
        },
        async abandon() {
          if (!retained) throw new Error("no retained descriptor");
          // Finite detached work: the result returns at once, but the callback completes
          // physically only after the 40 ms wait and the stat settle. The descriptor survives.
          // (A never-ending detached wait is the B1 deadline case, tests/source/b1.)
          void waitFor(40_000_000n).catch(() => {});
          void retained.stat().catch(() => {});
          return "returned";
        },
        async large() {
          // Streams a multi-mebibyte file chunk by chunk without holding it in the guest heap.
          const [directory] = root();
          const file = await directory.openAt({}, "large.bin", {}, { read: true });
          const [stream, done] = file.readViaStream(0n);
          let length = 0;
          let chunks = 0;
          let largest = 0;
          let matches = true;
          for await (const chunk of readChunks(stream)) {
            matches &&=
              chunk[0] === length % 251 &&
              chunk[chunk.length - 1] === (length + chunk.length - 1) % 251;
            length += chunk.length;
            largest = Math.max(largest, chunk.length);
            chunks++;
          }
          const result = await done.read();
          return JSON.stringify({ length, chunks, largest, matches, done: result.tag });
        },
        async denied() {
          const [directory] = root();
          return JSON.stringify({
            traversal: await refusal(directory.openAt({}, "../escape", {}, { read: true })),
            write: await refusal(directory.openAt({}, "bytes.bin", {}, { write: true })),
            create: await refusal(
              directory.openAt({}, "new.bin", { create: true }, { read: true }),
            ),
            missing: await refusal(directory.openAt({}, "missing.bin", {}, { read: true })),
          });
        },
      },
    },
  },
});
