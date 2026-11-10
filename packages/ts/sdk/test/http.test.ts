import { expect, test } from "bun:test";
import { decodeText, encodeText, Utf8Decoder } from "../src/http/bytes.ts";
import { events } from "../src/http/events.ts";

const encoder = new TextEncoder();

async function* bytes(parts: Uint8Array[]) {
  yield* parts;
}

test("maintained SSE parser emits split UTF-8 events before EOF", async () => {
  const input = encoder.encode(
    "\ufeff:heartbeat\r\nid: 42\r\nevent: token\r\ndata: hé🌍\r\ndata: second\r\n\r\nid: invalid\0id\ndata: third\n\ndata: incomplete",
  );
  let reads = 0;
  async function* source() {
    for (const byte of input) {
      reads++;
      yield Uint8Array.of(byte);
    }
  }
  const iterator = events(source())[Symbol.asyncIterator]();
  expect((await iterator.next()).value).toEqual({ event: "token", data: "hé🌍\nsecond", id: "42" });
  expect(reads).toBeLessThan(input.length);
  expect((await iterator.next()).value).toEqual({ event: "message", data: "third", id: "42" });
  expect((await iterator.next()).done).toBe(true);
});

test("early SSE return closes the byte producer", async () => {
  let closed = false;
  async function* source() {
    try {
      yield encodeText("data: first\n\n");
      yield encodeText("data: second\n\n");
    } finally {
      closed = true;
    }
  }
  for await (const event of events(source())) {
    expect(event.data).toBe("first");
    break;
  }
  expect(closed).toBe(true);
});

test("text collection enforces its byte bound and preserves producer errors", async () => {
  const input = encodeText("hé🌍");
  expect(await decodeText(bytes([input.subarray(0, 3), input.subarray(3)]), input.length)).toBe(
    "hé🌍",
  );
  await expect(decodeText(bytes([input]), input.length - 1)).rejects.toThrow("limit");
  const primary = new Error("read failed");
  async function* broken() {
    yield input;
    throw primary;
  }
  await expect(decodeText(broken(), input.length)).rejects.toBe(primary);
});

test("UTF-8 decoding agrees with the platform decoder across scalar and invalid-byte boundaries", () => {
  for (const bytes of [
    encoder.encode("plain hé🌍"),
    Uint8Array.from([0xe0, 0x80, 0x61, 0xf0, 0x90, 0x80]),
    Uint8Array.from([0xed, 0xa0, 0x80, 0xf4, 0x90, 0x80, 0x80]),
  ]) {
    for (let width = 1; width <= 4; width++) {
      const decoder = new Utf8Decoder();
      let text = "";
      for (let offset = 0; offset < bytes.length; offset += width)
        text += decoder.decode(bytes.subarray(offset, offset + width));
      text += decoder.decode(undefined, true);
      expect(text).toBe(new TextDecoder().decode(bytes));
    }
  }
});
