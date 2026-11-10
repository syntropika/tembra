import { expect, test } from "bun:test";
import { BrowserCapabilities } from "../src/interpreter/io.ts";

const encoder = new TextEncoder();
const request = {
  operation: "http.send",
  url: "https://api.example.test/events",
  method: "GET",
  headers: [],
  body: [],
} as const;

test("HTTP grants validate exact normalized origins using the browser URL API", async () => {
  for (const origin of [
    "https://*.example.test",
    "https://example.test/path",
    "https://example.test/?",
    "https://example.test/#",
    "file:///tmp",
    "https://user:password@example.test/",
  ])
    await expect(
      BrowserCapabilities.open({ http: [{ provider: "web", origins: [origin] }] }, {}),
    ).rejects.toThrow();
  const io = await BrowserCapabilities.open(
    { http: [{ provider: "web", origins: ["https://API.example.test:443"] }] },
    {
      http: { web: async () => new Response(null) },
    },
  );
  try {
    const callback = new AbortController();
    const response = (await io.execute(request, callback.signal)) as { id: number };
    await io.execute({ operation: "http.close", id: response.id });
    await expect(
      io.execute({ ...request, url: `${request.url}#` }, callback.signal),
    ).rejects.toThrow();
  } finally {
    await io.dispose();
  }
});

/** Install a Fetch-compatible test provider through the public host capability seam. */
test("HTTP provider streams without EOF, honors callback cancellation and rejects ungranted origins", async () => {
  let calls = 0;
  let cancelled = false;
  let sent!: ReadableStreamDefaultController<Uint8Array>;
  const callback = new AbortController();
  const io = await BrowserCapabilities.open(
    { http: [{ provider: "mock", origins: ["https://api.example.test"] }] },
    {
      http: {
        mock: async (_input, init) => {
          calls++;
          expect(init?.redirect).toBe("error");
          expect(init?.credentials).toBe("omit");
          return new Response(
            new ReadableStream({
              start(controller) {
                sent = controller;
              },
              cancel() {
                cancelled = true;
              },
            }),
            { headers: { "content-type": "text/event-stream" } },
          );
        },
      },
    },
  );
  try {
    for (const name of ["Host", "Transfer-Encoding", "Content-Length", "Connection"]) {
      await expect(
        io.execute({ ...request, headers: [[name, "forbidden"]] }, callback.signal),
      ).rejects.toThrow("set by the host");
    }
    expect(calls).toBe(0);
    for (const [name, value] of [
      ["bad\r\nHost", "evil"],
      ["x-test", "value\r\nHost: evil"],
    ])
      await expect(
        io.execute({ ...request, headers: [[name!, value!]] }, callback.signal),
      ).rejects.toThrow();
    await expect(
      io.execute({ ...request, url: "https://forbidden.example.test/" }, callback.signal),
    ).rejects.toThrow("granted");
    expect(calls).toBe(0);
    const response = (await io.execute(request, callback.signal)) as { id: number; status: number };
    expect(response.status).toBe(200);
    const reading = io.execute({ operation: "http.read", id: response.id }, callback.signal);
    sent.enqueue(encoder.encode("data: hello\n\n"));
    expect(await reading).toEqual([...encoder.encode("data: hello\n\n")]);
    const pending = io.execute({ operation: "http.read", id: response.id }, callback.signal);
    callback.abort();
    await pending;
    expect(cancelled).toBe(true);
    await expect(
      io.execute({ operation: "http.read", id: response.id }, new AbortController().signal),
    ).rejects.toThrow("Unknown");
    await io.execute({ operation: "http.close", id: response.id });
    await expect(io.execute({ operation: "http.close", id: response.id + 1 })).rejects.toThrow(
      "Unknown",
    );
  } finally {
    await io.dispose();
  }
});

test("HTTP and socket permissions remain independent for custom browser providers", async () => {
  let connections = 0;
  let closed = 0;
  const callback = new AbortController();
  const providers = {
    network: {
      mock: {
        async connect() {
          connections++;
          return {
            async read() {
              return Uint8Array.from({ length: 20000 }, () => 7);
            },
            async write() {},
            async close() {
              closed++;
            },
          };
        },
      },
    },
  };
  const httpOnly = await BrowserCapabilities.open(
    { http: [{ provider: "mock", origins: ["https://api.example.test"] }] },
    providers,
  );
  await expect(
    httpOnly.execute(
      { operation: "transport.connect", ip: "127.0.0.1", port: 80 },
      callback.signal,
    ),
  ).rejects.toThrow("TCP");
  expect(connections).toBe(0);
  await httpOnly.dispose();
  const io = await BrowserCapabilities.open(
    { network: [{ provider: "mock", connect: ["127.0.0.1:80"] }] },
    providers,
  );
  try {
    const id = (await io.execute(
      { operation: "transport.connect", ip: "127.0.0.1", port: 80 },
      callback.signal,
    )) as number;
    expect(
      ((await io.execute({ operation: "transport.read", id }, callback.signal)) as number[]).length,
    ).toBe(16384);
    expect(
      ((await io.execute({ operation: "transport.read", id }, callback.signal)) as number[]).length,
    ).toBe(3616);
    await expect(io.execute(request, callback.signal)).rejects.toThrow("granted");
    callback.abort();
    expect(closed).toBe(1);
  } finally {
    await io.dispose();
  }
});

test("a host provider resolving after cancellation cannot publish a live handle", async () => {
  let release!: () => void;
  let closed = 0;
  const callback = new AbortController();
  const io = await BrowserCapabilities.open(
    { network: [{ provider: "mock", connect: ["127.0.0.1:80"] }] },
    {
      network: {
        mock: {
          async connect() {
            await new Promise<void>((resolve) => {
              release = resolve;
            });
            return {
              async read() {
                return null;
              },
              async write() {},
              async close() {
                closed++;
              },
            };
          },
        },
      },
    },
  );
  try {
    const connection = io.execute(
      { operation: "transport.connect", ip: "127.0.0.1", port: 80 },
      callback.signal,
    );
    callback.abort();
    release();
    await expect(connection).rejects.toThrow();
    expect(closed).toBe(1);
  } finally {
    await io.dispose();
  }
});
