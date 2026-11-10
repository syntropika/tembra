// The public HTTP helper bundled with the real generated guest bindings and browser host.
// Providers own HTTP/TLS; this test exercises the binary resource/stream/future boundary.
import { expect, test } from "bun:test";
import { createContext, runInContext } from "node:vm";
import path from "node:path";
import { StandardHost } from "../src/interpreter/standard-host.ts";
import { fetchHttpProvider } from "../src/interpreter/fetch-http-provider.ts";
import { resolver } from "../src/tooling/resolver.ts";

const imports = new Set(["wasi:http/client@0.3.0", "wasi:http/types@0.3.0"]);
const bootstrap = await Bun.build({
  entrypoints: [
    path.resolve(import.meta.dir, "../../../../crates/executor-wasm/guest/src/standard.ts"),
  ],
  format: "iife",
  target: "node",
});
const helper = await Bun.build({
  entrypoints: ["tembra:test-http"],
  plugins: [
    {
      name: "test-entry",
      setup(builder) {
        builder.onResolve({ filter: /^tembra:test-http$/ }, () => ({
          path: "http",
          namespace: "test-entry",
        }));
        builder.onLoad({ filter: /.*/, namespace: "test-entry" }, () => ({
          contents: `import * as http from ${JSON.stringify(path.resolve(import.meta.dir, "../src/http.ts"))}; globalThis.__httpModule = http;`,
          loader: "js",
        }));
      },
    },
    resolver(
      path.resolve(import.meta.dir, ".."),
      { entry: "src/http.ts", wit: "wit/http.wit" },
      imports,
    ),
  ],
  format: "iife",
  target: "browser",
});
for (const built of [bootstrap, helper]) if (!built.success) throw new AggregateError(built.logs);
const scripts = await Promise.all([bootstrap, helper].map((built) => built.outputs[0]!.text()));

async function guest(fetch: NonNullable<Parameters<typeof fetchHttpProvider>[0]>) {
  const provider = fetchHttpProvider(fetch);
  const standard = new StandardHost(imports, [], 8 << 20, {
    http: { provider: (url) => (url.origin === "https://example.test" ? provider : undefined) },
  });
  const globals: any = {
    Uint8Array,
    DataView,
    Symbol,
    __tembraContract: { imports: [...imports].map((contract) => ({ contract, type_only: false })) },
    __hostSyncBytes: (packet: Uint8Array) => standard.sync(packet),
    __hostAsync: (packet: Uint8Array) => standard.async(packet).then((reply) => reply.bytes),
  };
  const context = createContext(globals);
  for (const script of scripts) runInContext(script!, context);
  return {
    fetch: globals.__httpModule.fetch as typeof import("../src/http.ts").fetch,
    types: globals.__tembraStandard[
      "wasi:http/types@0.3.0"
    ] as typeof import("wasi:http/types@0.3.0"),
    standard,
    async boundary() {
      globals.__tembraSweepStandard(true);
      standard.endCallback();
      await standard.settled();
    },
    async dispose() {
      globals.__tembraSweepStandard(true);
      await standard.dispose();
      expect(standard.size).toBe(0);
      expect(standard.inputBytes).toBe(0);
    },
  };
}

test("HTTP helper sends a standard request and consumes a streamed body once", async () => {
  const calls: { url: string; init: RequestInit }[] = [];
  const engine = await guest(async (url, init) => {
    calls.push({ url: String(url), init: init! });
    return new Response('"hé🌍"', { status: 201, headers: { "content-type": "application/json" } });
  });
  try {
    const response = await engine.fetch("https://example.test/path?q=2&q=1", {
      method: "POST",
      body: "hé🌍",
      headers: { "Content-Type": "text/plain" },
    });
    expect(response.status).toBe(201);
    expect(response.ok).toBe(true);
    expect(await response.json()).toBe("hé🌍");
    expect(calls).toHaveLength(1);
    expect(calls[0]!.url).toBe("https://example.test/path?q=2&q=1");
    expect(calls[0]!.init.method).toBe("POST");
    expect(calls[0]!.init.body).toEqual(new TextEncoder().encode("hé🌍"));
    expect(calls[0]!.init.redirect).toBe("manual");
    expect([...(await response.trailers())]).toEqual([]);
    await expect(response.text()).rejects.toThrow("already been consumed");
    await engine.boundary();
  } finally {
    await engine.dispose();
  }
});

test("an untouched HTTP response survives a callback boundary", async () => {
  const engine = await guest(async () => new Response("retained"));
  try {
    const response = await engine.fetch("https://example.test/retained");
    await engine.boundary();
    expect(await response.text()).toBe("retained");
    await engine.boundary();
  } finally {
    await engine.dispose();
  }
});

test("closing an untouched response cancels its host body once", async () => {
  let closes = 0;
  const engine = await guest(
    async () =>
      new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(new TextEncoder().encode("data: first\n\n"));
          },
          cancel() {
            closes++;
          },
        }),
        { headers: { "content-type": "text/event-stream" } },
      ),
  );
  try {
    const response = await engine.fetch("https://example.test/events");
    await response.close();
    await response.close();
    await engine.boundary();
    expect(closes).toBe(1);
    await expect(response.text()).rejects.toThrow("closed");
  } finally {
    await engine.dispose();
  }
});

test("a throwing request producer never commits a truncated upload to Fetch", async () => {
  let sent = 0;
  const engine = await guest(async () => {
    sent++;
    return new Response("unexpected");
  });
  async function* broken() {
    yield new Uint8Array(1000);
    throw new Error("producer failed");
  }
  try {
    await expect(
      engine.fetch("https://example.test/upload", {
        method: "POST",
        body: broken(),
        trailers: Promise.resolve({ "x-finished": "unused" }),
      }),
    ).rejects.toThrow();
    expect(sent).toBe(0);
    await engine.boundary();
    expect(engine.standard.size).toBe(0);
  } finally {
    await engine.dispose();
  }
});

test("grant denial and invalid requests do not call the HTTP provider", async () => {
  let sent = 0;
  const engine = await guest(async () => {
    sent++;
    return new Response("unexpected");
  });
  try {
    await expect(engine.fetch("https://denied.test/")).rejects.toThrow("HTTP-request-denied");
    for (const method of ["CONNECT", "trace", "TRACK"])
      await expect(engine.fetch("https://example.test/", { method })).rejects.toThrow("forbidden");
    for (const method of ["get", "HEAD"])
      await expect(engine.fetch("https://example.test/", { method, body: "" })).rejects.toThrow(
        "body",
      );
    for (const url of ["relative", "ftp://example.test/", "https://user:pass@example.test/"])
      await expect(engine.fetch(url)).rejects.toThrow("absolute");
    expect(sent).toBe(0);
    await engine.boundary();
  } finally {
    await engine.dispose();
  }
});

test("generated resource constructors and statics preserve ownership on a refused argument", async () => {
  const engine = await guest(async () => new Response("unused"));
  try {
    const { Fields, Request, RequestOptions } = engine.types;
    const fields = new Fields();
    fields.append("x-raw", Uint8Array.of(255));
    expect(fields.copyAll()).toEqual([["x-raw", Uint8Array.of(255)]]);
    expect(Fields.name).toBe("Fields");
    const baseline = engine.standard.size;
    // The owned headers precede the bad body: failed argument encoding must not move them.
    expect(() =>
      Request.new(
        fields,
        {} as AsyncIterable<number>,
        Promise.resolve({ tag: "ok", val: undefined }),
        undefined,
      ),
    ).toThrow("AsyncIterable");
    expect(fields.get("x-raw")).toEqual([Uint8Array.of(255)]);
    expect(engine.standard.size).toBe(baseline);
    const options = new RequestOptions();
    options.setFirstByteTimeout(123456n);
    expect(options.getFirstByteTimeout()).toBe(123456n);
    const copy = options.clone();
    expect(copy.getFirstByteTimeout()).toBe(123456n);
    copy[Symbol.dispose]();
    options[Symbol.dispose]();
    fields[Symbol.dispose]();
    expect(engine.standard.size).toBe(0);
  } finally {
    await engine.dispose();
  }
});

test("standard request and header refusals preserve typed errors without a TypeError", async () => {
  let calls = 0;
  const engine = await guest(async () => {
    calls++;
    return new Response("unexpected");
  });
  try {
    for (const [url, options, code] of [
      ["https://example.test/a b", {}, undefined],
      ["https://example.test/a?q=1 2", {}, undefined],
      ["https://example.test/", { method: "BAD METHOD" }, undefined],
      ["https://example.test/", { headers: { "bad name": "x" } }, "invalid-syntax"],
    ] as const) {
      const error = (await engine.fetch(url, options).catch((error: unknown) => error)) as {
        name: string;
        code: { tag: string } | undefined;
        message: string;
      };
      expect(error.name).toBe("HttpError");
      expect(error.code?.tag).toBe(code);
      expect(error.message).toContain(code ?? "request rejected");
    }
    await expect(engine.fetch("https://exa mple.test/")).rejects.toThrow(
      "host contains whitespace",
    );
    const refused = (await engine
      .fetch("https://example.test/", { timeouts: { connect: 1n } })
      .catch((error: unknown) => error)) as { name: string; code: { tag: string } };
    expect(refused.name).toBe("HttpError");
    expect(refused.code.tag).toBe("internal-error");
    expect(calls).toBe(0);
    await engine.boundary();
    expect(engine.standard.size).toBe(0);
  } finally {
    await engine.dispose();
  }
});

test("a byte body is captured when fetch is called", async () => {
  let sent: Uint8Array | undefined;
  const engine = await guest(async (_url, init) => {
    sent = init.body as Uint8Array;
    return new Response("ok");
  });
  try {
    const bytes = Uint8Array.of(1, 2, 3);
    const pending = engine.fetch("https://example.test/", { method: "POST", body: bytes });
    bytes.fill(0);
    expect(await (await pending).text()).toBe("ok");
    expect(sent).toEqual(Uint8Array.of(1, 2, 3));
    await engine.boundary();
  } finally {
    await engine.dispose();
  }
});
