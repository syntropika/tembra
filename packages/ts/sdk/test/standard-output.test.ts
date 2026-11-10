import { expect, test } from "bun:test";
import { OutputStream } from "../src/interpreter/standard-streams.ts";
import { Reader, Writer } from "../src/interpreter/standard-codec.ts";

/** Cancel exactly while a typed batch has a produced prefix and one pending item. */
test("cancelling a typed batch preserves its collected prefix before the pending item", async () => {
  let enter!: () => void, release!: () => void;
  const entered = new Promise<void>((resolve) => {
    enter = resolve;
  });
  const blocked = new Promise<void>((resolve) => {
    release = resolve;
  });
  let position = 0,
    closed = 0;
  const stream = new OutputStream(
    {
      async next() {
        if (position === 2) {
          enter();
          await blocked;
        }
        return position < 5 ? position++ : undefined;
      },
      close() {
        closed++;
      },
    },
    () => {},
  );
  const encode = (value: unknown) => {
    const writer = new Writer();
    writer.u32(value as number);
    return writer.finish();
  };
  const controller = new AbortController();
  const pending = stream
    .read(256, 65536, controller.signal, encode)
    .catch((error) => String(error));
  await entered;
  controller.abort();
  expect(await pending).toContain("cancelled");
  release();
  const seen: number[] = [];
  for (;;) {
    const reader = new Reader(await stream.read(256, 65536, undefined, encode));
    if (reader.u8() === 1) {
      reader.end();
      break;
    }
    for (let count = reader.u32(); count > 0; count--) seen.push(reader.u32());
    reader.end();
  }
  expect(seen).toEqual([0, 1, 2, 3, 4]);
  expect(closed).toBe(1);
  stream.close();
  expect(closed).toBe(1);
});
