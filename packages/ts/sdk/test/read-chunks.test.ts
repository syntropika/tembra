import { expect, test } from "bun:test";
import { readChunks, writeChunks } from "../src/guest.ts";

/** A lifted `stream<u8>` as the component runtime exposes it: chunks plus an explicit drop. */
function lifted(chunks: Uint8Array[]) {
  let dropped = 0;
  const stream = {
    drop() {
      dropped++;
    },
    async *[Symbol.asyncIterator]() {
      yield* chunks;
    },
  };
  return { stream, dropped: () => dropped };
}

test("readChunks yields the runtime's chunks unchanged and drops the stream", async () => {
  const first = Uint8Array.of(1, 2);
  const second = Uint8Array.of(3);
  const { stream, dropped } = lifted([first, second]);
  const seen: Uint8Array[] = [];
  for await (const chunk of readChunks(stream)) seen.push(chunk);
  expect(seen).toEqual([first, second]);
  expect(seen[0]).toBe(first);
  expect(dropped()).toBe(1);
});

test("readChunks drops the stream when the reader stops early", async () => {
  const { stream, dropped } = lifted([Uint8Array.of(1), Uint8Array.of(2)]);
  const reader = readChunks(stream);
  await reader.next();
  await reader.return();
  expect(dropped()).toBe(1);
});

test("writeChunks passes async sources through and adapts synchronous ones lazily", async () => {
  const source = (async function* () {
    yield Uint8Array.of(9);
  })();
  expect(writeChunks(source)).toBe(source);
  let pulled = 0;
  function* chunks() {
    pulled++;
    yield Uint8Array.of(4, 5);
  }
  const adapted = writeChunks(chunks());
  expect(pulled).toBe(0);
  const seen: Uint8Array[] = [];
  for await (const chunk of adapted) seen.push(chunk);
  expect(seen).toEqual([Uint8Array.of(4, 5)]);
});
