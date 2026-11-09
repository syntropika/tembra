import { expect, test } from "bun:test";
import { fetch, type TransportStream } from "../src/http.ts";
import { Utf8Decoder } from "../src/http/bytes.ts";

const encoder = new TextEncoder();

/** Supply bytes only; all HTTP framing and text decoding still run in the client under test. */
async function transport<T>(
  chunks: Uint8Array[],
  run: (state: { writes: Uint8Array[]; reads: number; closes: number }) => Promise<T>,
  closeFailure?: Error,
): Promise<T> {
  const previous = Object.getOwnPropertyDescriptor(globalThis, "tembra");
  const state = { writes: [] as Uint8Array[], reads: 0, closes: 0 };
  const stream: TransportStream = {
    async read() {
      state.reads++;
      return chunks.shift() ?? null;
    },
    async write(bytes) {
      state.writes.push(bytes.slice());
    },
    async close() {
      state.closes++;
      if (closeFailure) throw closeFailure;
    },
  };
  Object.defineProperty(globalThis, "tembra", {
    configurable: true,
    value: {
      transport: {
        async connect(ip: string, port: number) {
          expect(ip).toBe("127.0.0.1");
          expect(port).toBe(8000);
          return stream;
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

test("HTTP handles split framing, chunk extensions, trailers and UTF-8 POST bodies", async () => {
  const body = encoder.encode('"hé🌍"');
  const raw = encoder.encode(
    `HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n${body.length.toString(16)};a=b\r\n"hé🌍"\r\n0\r\nX-Finished: yes\r\n\r\n`,
  );
  await transport(
    [...raw].map((byte) => Uint8Array.of(byte)),
    async (state) => {
      const response = await fetch("http://127.0.0.1:8000/path?q=1", {
        method: "POST",
        body: "hé🌍",
        headers: { "Content-Type": "text/plain" },
      });
      expect(response.status).toBe(200);
      expect(await response.json()).toBe("hé🌍");
      await expect(response.text()).rejects.toThrow("already been consumed");
      expect(state.closes).toBe(1);
      const request = new TextDecoder().decode(
        Uint8Array.from(state.writes.flatMap((bytes) => [...bytes])),
      );
      expect(request).toStartWith("POST /path?q=1 HTTP/1.1\r\n");
      expect(request).toContain("Content-Length: 7\r\n");
      expect(request).toEndWith("\r\n\r\nhé🌍");
    },
  );
});

test("HTTP preserves opaque response header bytes and percent-encodes request targets", async () => {
  const response = Uint8Array.from([
    ...encoder.encode("HTTP/1.1 200 "),
    0xe9,
    ...encoder.encode("\r\nX-Name: caf"),
    0xe9,
    ...encoder.encode("\r\nX-Opaque: "),
    0xa0,
    0x61,
    0xa0,
    ...encoder.encode("\r\nContent-Length: 0\r\n\r\n"),
  ]);
  await transport([response], async (state) => {
    const result = await fetch("http://127.0.0.1:8000/café/🌍/\x7f?q=ñ", { method: "POST" });
    expect(result.headers.get("x-name")).toBe("caf\xe9");
    expect(result.headers.get("x-opaque")).toBe("\xa0a\xa0");
    expect(await result.text()).toBe("");
    const request = new TextDecoder().decode(state.writes[0]);
    expect(request).toStartWith("POST /caf%C3%A9/%F0%9F%8C%8D/%7F?q=%C3%B1 HTTP/1.1\r\n");
    expect(request).toContain("Content-Length: 0\r\n");
  });
});

test("HTTP supports known lengths, EOF bodies and empty HEAD/204/304 responses", async () => {
  for (const [status, method, framing, expected] of [
    [200, "GET", "Content-Length: 5\r\n", "hello"],
    [200, "GET", "", "hello"],
    [200, "HEAD", "Content-Length: 5\r\n", ""],
    [204, "GET", "", ""],
    [304, "GET", "", ""],
  ] as const) {
    await transport(
      [encoder.encode(`HTTP/1.1 ${status} OK\r\n${framing}\r\nhello`)],
      async (state) => {
        const response = await fetch("http://127.0.0.1:8000/", { method });
        if (!expected) expect(state.closes).toBe(1);
        expect(await response.text()).toBe(expected);
        expect(state.closes).toBe(1);
      },
    );
  }
});

test("HTTP cleanup cannot replace a body failure or turn a successful SSE break into a failure", async () => {
  const cleanup = new Error("cleanup failed");
  await transport(
    [encoder.encode("HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nshort")],
    async () => {
      const response = await fetch("http://127.0.0.1:8000/");
      await expect(response.text()).rejects.toThrow("Truncated HTTP body");
      await expect(response.close()).rejects.toBe(cleanup);
    },
    cleanup,
  );
  await transport(
    [encoder.encode("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: first\n\n")],
    async (state) => {
      const response = await fetch("http://127.0.0.1:8000/");
      for await (const event of response.events()) {
        expect(event.data).toBe("first");
        break;
      }
      expect(state.closes).toBe(1);
    },
    cleanup,
  );
  await transport(
    [encoder.encode("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{}")],
    async (state) => {
      const response = await fetch("http://127.0.0.1:8000/");
      await expect(response.events()[Symbol.asyncIterator]().next()).rejects.toThrow(
        "SSE requires",
      );
      expect(state.closes).toBe(1);
    },
  );
});

test("SSE yields before EOF, preserves event IDs and closes when the consumer stops", async () => {
  const raw = encoder.encode(
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\r\n\ufeff:heartbeat\r\nid: 42\r\nevent: token\r\ndata: hé🌍\r\ndata: second\r\n\r\nid: invalid\0id\ndata: third\n\n",
  );
  await transport(
    [...raw].map((byte) => Uint8Array.of(byte)),
    async (state) => {
      const response = await fetch("http://127.0.0.1:8000/events");
      const iterator = response.events()[Symbol.asyncIterator]();
      expect((await iterator.next()).value).toEqual({
        event: "token",
        data: "hé🌍\nsecond",
        id: "42",
      });
      expect(state.reads).toBeLessThan(raw.length);
      expect(state.closes).toBe(0);
      expect((await iterator.next()).value).toEqual({ event: "message", data: "third", id: "42" });
      await iterator.return?.();
      expect(state.closes).toBe(1);
    },
  );
});

test("HTTP refuses ambiguous framing and closes truncated bodies", async () => {
  for (const headers of [
    "Content-Length: 1\r\nContent-Length: 1",
    "Content-Length: 1\r\nTransfer-Encoding: chunked",
    "Content-Length: -1",
    "Transfer-Encoding: gzip, chunked",
  ]) {
    await transport(
      [encoder.encode(`HTTP/1.1 200 OK\r\n${headers}\r\n\r\n0\r\n\r\n`)],
      async (state) => {
        await expect(fetch("http://127.0.0.1:8000/")).rejects.toThrow();
        expect(state.closes).toBe(1);
      },
    );
  }
  await transport(
    [encoder.encode("HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\nshort")],
    async (state) => {
      const response = await fetch("http://127.0.0.1:8000/");
      await expect(response.text()).rejects.toThrow("Truncated HTTP body");
      expect(state.closes).toBe(1);
    },
  );
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
