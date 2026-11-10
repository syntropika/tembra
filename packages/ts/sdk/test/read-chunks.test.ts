import { expect, test } from "bun:test";
import { readChunks, writeChunks } from "../src/guest.ts";

const READ_CHUNK = Symbol.for("tembra.wasi.stream.read-chunk");

test("readChunks returns interpreter chunks unchanged without per-byte iteration", async () => {
  const chunks = [Uint8Array.of(0, 255, 128), Uint8Array.of(7)];
  let iterated = false;
  const stream = {
    async [READ_CHUNK]() {
      return chunks.shift();
    },
    async *[Symbol.asyncIterator](): AsyncGenerator<number> {
      iterated = true;
      yield 0;
    },
  };
  const read: Uint8Array[] = [];
  for await (const chunk of readChunks(stream)) read.push(chunk);
  expect(read).toEqual([Uint8Array.of(0, 255, 128), Uint8Array.of(7)]);
  expect(iterated).toBe(false);
});

test("readChunks batches any other byte iterable into bounded chunks", async () => {
  async function* bytes(): AsyncGenerator<number> {
    for (let index = 0; index < (64 << 10) + 3; index++) yield index % 256;
  }
  const read: Uint8Array[] = [];
  for await (const chunk of readChunks(bytes())) read.push(chunk);
  expect(read.map((chunk) => chunk.length)).toEqual([64 << 10, 3]);
  expect(Array.from(read[1]!)).toEqual([0, 1, 2]);
});

test("writeChunks exposes chunks to the interpreter and bytes to any other consumer", async () => {
  const WRITE_CHUNKS = Symbol.for("tembra.wasi.stream.write-chunks");
  const source = writeChunks([Uint8Array.of(1, 2), Uint8Array.of(3)]);
  const chunks = (source as unknown as Record<symbol, () => Iterator<Uint8Array>>)[WRITE_CHUNKS]!();
  expect([chunks.next().value, chunks.next().value, chunks.next().done]).toEqual([
    Uint8Array.of(1, 2),
    Uint8Array.of(3),
    true,
  ]);
  const bytes: number[] = [];
  for await (const byte of source) bytes.push(byte);
  expect(bytes).toEqual([1, 2, 3]);
  // readChunks over a non-interpreter iterable still batches correctly.
  const read: Uint8Array[] = [];
  for await (const chunk of readChunks(
    writeChunks(
      (async function* () {
        yield Uint8Array.of(9);
      })(),
    ),
  ))
    read.push(chunk);
  expect(read).toEqual([Uint8Array.of(9)]);
});
