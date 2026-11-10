import { expect, test } from "bun:test";
import { fetch } from "../src/http.ts";
import type { HttpStream } from "../src/guest.ts";
import { Utf8Decoder } from "../src/http/bytes.ts";

const encoder = new TextEncoder();

/** Model the host's decoded response, never a second HTTP framing implementation. */
async function host<T>(
  chunks: Uint8Array[],
  run: (state: { calls: unknown[]; reads: number; closes: number }) => Promise<T>,
  options: {
    headers?: [string, string][];
    status?: number;
    closeError?: Error;
    readError?: Error;
  } = {},
): Promise<T> {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "tembra");
  const state = { calls: [] as unknown[], reads: 0, closes: 0 };
  Object.defineProperty(globalThis, "tembra", {
    configurable: true,
    value: {
      http: {
        async send(...args: unknown[]): Promise<HttpStream> {
          state.calls.push(args);
          return {
            status: options.status ?? 200,
            headers: options.headers ?? [["content-type", "application/json"]],
            async read() {
              state.reads++;
              if (options.readError) throw options.readError;
              return chunks.shift() ?? null;
            },
            async close() {
              state.closes++;
              if (options.closeError) throw options.closeError;
            },
          };
        },
      },
    },
  });
  try {
    return await run(state);
  } finally {
    if (previous) Object.defineProperty(globalThis, "tembra", previous);
    else Reflect.deleteProperty(globalThis, "tembra");
  }
}

test("fetch forwards semantic request data and consumes a host-decoded response once", async () => {
  await host([encoder.encode('"hé🌍"')], async (state) => {
    const response = await fetch("https://example.test/path", {
      method: "POST",
      body: "hé🌍",
      headers: { "Content-Type": "text/plain" },
    });
    expect(response.status).toBe(200);
    expect(await response.json()).toBe("hé🌍");
    expect(state.calls).toEqual([
      [
        "https://example.test/path",
        "POST",
        [["Content-Type", "text/plain"]],
        encoder.encode("hé🌍"),
      ],
    ]);
    expect(state.closes).toBe(1);
    await expect(response.text()).rejects.toThrow("already been consumed");
  });
});

test("HTTP bodies stream before EOF and early iteration closes the host handle", async () => {
  await host([Uint8Array.of(1), Uint8Array.of(2)], async (state) => {
    const response = await fetch("https://example.test/bytes");
    for await (const bytes of response.body) {
      expect(bytes).toEqual(Uint8Array.of(1));
      break;
    }
    expect(state.reads).toBe(1);
    expect(state.closes).toBe(1);
    await response.close();
    expect(state.closes).toBe(1);
  });
});

test("fetch refuses browser-forbidden methods and GET/HEAD bodies before host I/O", async () => {
  await host([], async (state) => {
    for (const method of ["get", "HEAD"])
      await expect(fetch("https://example.test/", { method, body: "" })).rejects.toThrow("body");
    for (const method of ["CONNECT", "trace", "TRACK"])
      await expect(fetch("https://example.test/", { method })).rejects.toThrow("forbidden");
    expect(state.calls).toHaveLength(0);
  });
});

test("host read failures retain their cause when close also fails", async () => {
  const primary = new Error("read failed");
  await host(
    [],
    async (state) => {
      const response = await fetch("https://example.test/");
      await expect(response.text()).rejects.toBe(primary);
      expect(state.closes).toBe(1);
    },
    { readError: primary, closeError: new Error("close failed") },
  );
});

test("maintained SSE parser handles split UTF-8, CRLF, IDs and incomplete EOF", async () => {
  const bytes = encoder.encode(
    "\ufeff:heartbeat\r\nid: 42\r\nevent: token\r\ndata: hé🌍\r\ndata: second\r\n\r\nid: invalid\0id\ndata: third\n\ndata: incomplete",
  );
  await host(
    [...bytes].map((byte) => Uint8Array.of(byte)),
    async (state) => {
      const response = await fetch("https://example.test/events");
      const iterator = response.events()[Symbol.asyncIterator]();
      expect((await iterator.next()).value).toEqual({
        event: "token",
        data: "hé🌍\nsecond",
        id: "42",
      });
      expect(state.reads).toBeLessThan(bytes.length);
      expect((await iterator.next()).value).toEqual({ event: "message", data: "third", id: "42" });
      expect((await iterator.next()).done).toBe(true);
      expect(state.closes).toBe(1);
    },
    { headers: [["Content-Type", "text/event-stream; charset=utf-8"]] },
  );
});

test("early SSE return preserves success when close fails; wrong content types close too", async () => {
  await host(
    [encoder.encode("data: first\n\n"), encoder.encode("data: second\n\n")],
    async (state) => {
      const response = await fetch("https://example.test/events");
      for await (const event of response.events()) {
        expect(event.data).toBe("first");
        break;
      }
      expect(state.reads).toBe(1);
      expect(state.closes).toBe(1);
    },
    { headers: [["content-type", "text/event-stream"]], closeError: new Error("cleanup failed") },
  );
  await host([], async (state) => {
    const response = await fetch("https://example.test/");
    await expect(response.events()[Symbol.asyncIterator]().next()).rejects.toThrow(
      "text/event-stream",
    );
    expect(state.closes).toBe(1);
  });
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
