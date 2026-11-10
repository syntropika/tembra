import { expect, test } from "bun:test";
import { fetchHttpProvider } from "../src/interpreter/http-provider.ts";
import { decodeAsyncHostRequest } from "../src/interpreter/host-protocol.ts";
import { BrowserCapabilities } from "../src/interpreter/io.ts";

const web = fetchHttpProvider(async () => new Response(null));

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
    { http: { web } },
  );
  try {
    // Standard `wasi:http` sends select a provider only for the exact granted origin.
    expect(io.network.provider(new URL("https://api.example.test/events"))).toBe(web);
    for (const url of [
      "http://api.example.test/",
      "https://api.example.test:8443/",
      "https://other.example.test/",
      "https://user@api.example.test/",
    ])
      expect(io.network.provider(new URL(url))).toBeUndefined();
    // The legacy JSON HTTP operations no longer exist: a hostile packet is refused at decode.
    for (const operation of ["http.send", "http.read", "http.close"])
      expect(() =>
        decodeAsyncHostRequest(
          JSON.stringify({ operation, url: "https://api.example.test/", id: 1 }),
        ),
      ).toThrow(`Unsupported asynchronous host request operation ${operation}`);
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
    expect(io.network.provider(new URL("https://api.example.test/"))).toBeUndefined();
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
