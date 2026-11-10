// Browser StandardHost outgoing `wasi:http` (wire v7) through the default Fetch adapter.
// Raw packets exercise the exact wire the generated guest bindings use. Providers are local and
// deterministic; no test touches the network.
import { expect, test } from "bun:test";
import {
  FETCH_BODY_BYTES,
  fetchHttpProvider,
  HttpProviderError,
} from "../src/interpreter/http-provider.ts";
import { METHODS } from "../src/interpreter/standard.ts";
import { httpMethods } from "../src/interpreter/standard-http.ts";
import { methodKey } from "../src/interpreter/standard-streams.ts";
import {
  CLIENT,
  decoder,
  deferred,
  encoder,
  HTTP,
  ORIGIN,
  OUT_TRAILERS,
  RES,
  RESULT,
  retire,
  standard as host,
  text,
  tick,
  TRAILERS,
} from "./standard-guest.ts";

/** A host whose granted provider is `fetchHttpProvider` over `fetch` (or none). */
const standard = (fetch?: (input: URL, init: RequestInit) => Promise<Response>) =>
  host(fetch && fetchHttpProvider(fetch));

test("every generated wasi:http function is bound by its resource-qualified key", () => {
  const implementations = httpMethods(undefined);
  const generated = METHODS.filter((m) => m.interface === HTTP || m.interface === CLIENT);
  expect(generated.length).toBe(36);
  expect(new Set(generated.map((m) => m.resource).filter((r) => r !== undefined)).size).toBe(4);
  for (const m of generated) expect(implementations[methodKey(m)]).toBeFunction();
});

test("fields keep raw bytes, duplicates and upstream validation, mutability and limits", async () => {
  const { host, guest } = standard();
  const fields = guest.fields([
    ["X-Raw", [0xff, 0x80, 0x41]],
    ["x-dup", "one"],
    ["X-Dup", "two"],
  ]);
  expect(guest.sync("fields", "get", fields, "x-raw")).toEqual([Uint8Array.of(0xff, 0x80, 0x41)]);
  expect(guest.sync("fields", "get", fields, "X-DUP")).toEqual([
    encoder.encode("one"),
    encoder.encode("two"),
  ]);
  expect(text(guest.sync("fields", "copyAll", fields))).toEqual([
    ["x-raw", "��A"],
    ["x-dup", "one"],
    ["x-dup", "two"],
  ]);
  expect(guest.sync("fields", "has", fields, "X-DUP")).toBe(true);
  expect(guest.sync("fields", "has", fields, "bad name")).toBe(false);
  expect(guest.sync("fields", "get", fields, "bad name")).toEqual([]);
  for (const name of ["Host", "connection", "Transfer-Encoding", "upgrade", "http2-settings"])
    expect(guest.sync("fields", "append", fields, name, encoder.encode("x"))).toEqual({
      tag: "err",
      val: { tag: "forbidden" },
    });
  for (const [name, value] of [
    ["bad name", [0x41]],
    ["", [0x41]],
    ["x-ok", [0x41, 0x0d, 0x0a]],
    ["x-ok", [0x00]],
    ["x-ok", [0x7f]],
  ] as const)
    expect(guest.sync("fields", "append", fields, name, Uint8Array.from(value))).toEqual({
      tag: "err",
      val: { tag: "invalid-syntax" },
    });
  expect(guest.sync("fields", "getAndDelete", fields, "bad name")).toEqual({
    tag: "err",
    val: { tag: "invalid-syntax" },
  });
  // Content-Length is exactly one run of digits, stored normalized.
  expect(guest.sync("fields", "append", fields, "content-length", encoder.encode("+5")).tag).toBe(
    "err",
  );
  expect(
    guest.sync("fields", "set", fields, "content-length", [
      encoder.encode("1"),
      encoder.encode("2"),
    ]).tag,
  ).toBe("err");
  expect(guest.sync("fields", "append", fields, "Content-Length", encoder.encode("007")).tag).toBe(
    "ok",
  );
  expect(guest.sync("fields", "get", fields, "content-length")).toEqual([encoder.encode("7")]);
  expect(guest.sync("fields", "append", fields, "content-length", encoder.encode("7")).tag).toBe(
    "err",
  );
  // set replaces every value; an empty list removes the name; getAndDelete returns the values.
  expect(guest.sync("fields", "set", fields, "x-dup", [encoder.encode("three")]).tag).toBe("ok");
  expect(guest.sync("fields", "get", fields, "x-dup")).toEqual([encoder.encode("three")]);
  expect(guest.sync("fields", "getAndDelete", fields, "X-Dup")).toEqual({
    tag: "ok",
    val: [encoder.encode("three")],
  });
  expect(guest.sync("fields", "set", fields, "x-raw", []).tag).toBe("ok");
  expect(guest.sync("fields", "has", fields, "x-raw")).toBe(false);
  expect(guest.sync("fields", "delete", fields, "absent")).toEqual({ tag: "ok" });
  // The 128 KiB field budget refuses instead of growing.
  const big = new Uint8Array(60 << 10).fill(0x61);
  expect(guest.sync("fields", "append", fields, "x-big", big).tag).toBe("ok");
  expect(guest.sync("fields", "append", fields, "x-big", big).tag).toBe("ok");
  expect(guest.sync("fields", "append", fields, "x-big", big)).toEqual({
    tag: "err",
    val: { tag: "size-exceeded" },
  });
  expect(guest.sync("fields", "fromList", [["host", encoder.encode("evil")]])).toEqual({
    tag: "err",
    val: { tag: "forbidden" },
  });
  // A clone is a deep mutable copy.
  const clone = guest.sync("fields", "clone", fields);
  expect(guest.sync("fields", "delete", clone, "x-big").tag).toBe("ok");
  expect(guest.sync("fields", "get", fields, "x-big").length).toBe(2);
  // Headers moved into a request become immutable; views are immutable, their clones are not.
  const empty = guest.sync("fields", "constructor");
  expect(guest.sync("fields", "append", empty, "x-a", encoder.encode("1")).tag).toBe("ok");
  const trailers = guest.futureNew(TRAILERS);
  const [request] = guest.sync("request", "new", empty, undefined, trailers, undefined);
  expect(() => guest.sync("fields", "get", empty, "x-a")).toThrow("unknown or dropped handle");
  const view = guest.sync("request", "getHeaders", request);
  expect(text(guest.sync("fields", "copyAll", view))).toEqual([["x-a", "1"]]);
  for (const [name, args] of [
    ["set", ["x-a", [encoder.encode("2")]]],
    ["append", ["x-b", encoder.encode("2")]],
    ["delete", ["x-a"]],
    ["getAndDelete", ["x-a"]],
  ] as const)
    expect(guest.sync("fields", name, view, ...args)).toEqual({
      tag: "err",
      val: { tag: "immutable" },
    });
  const mutable = guest.sync("fields", "clone", view);
  expect(guest.sync("fields", "append", mutable, "x-b", encoder.encode("2")).tag).toBe("ok");
  await retire(host);
});

test("request-options: timeouts reach the provider; Fetch refuses a connect timeout; views are immutable", async () => {
  let calls = 0;
  const { host, guest } = standard(async () => {
    calls++;
    return new Response(null);
  });
  const options = guest.sync("request-options", "constructor");
  expect(guest.sync("request-options", "getConnectTimeout", options)).toBeUndefined();
  expect(guest.sync("request-options", "setConnectTimeout", options, 5n)).toEqual({ tag: "ok" });
  expect(guest.sync("request-options", "getConnectTimeout", options)).toBe(5n);
  const connecting = guest.request("/", {
    options: guest.sync("request-options", "clone", options),
  });
  expect(await guest.send(connecting.request, new AbortController().signal)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "browser Fetch has no separate connect timeout" },
  });
  expect(calls).toBe(0);
  expect(guest.sync("request-options", "setConnectTimeout", options, undefined)).toEqual({
    tag: "ok",
  });
  expect(guest.sync("request-options", "setFirstByteTimeout", options, 7n).tag).toBe("ok");
  expect(guest.sync("request-options", "setBetweenBytesTimeout", options, 9n).tag).toBe("ok");
  expect(guest.sync("request-options", "getFirstByteTimeout", options)).toBe(7n);
  const copy = guest.sync("request-options", "clone", options);
  expect(guest.sync("request-options", "setFirstByteTimeout", copy, 1n).tag).toBe("ok");
  expect(guest.sync("request-options", "getFirstByteTimeout", options)).toBe(7n);
  const { request } = guest.request("/", { options });
  expect(() => guest.sync("request-options", "clone", options)).toThrow("unknown or dropped");
  const view = guest.sync("request", "getOptions", request);
  expect(guest.sync("request-options", "getBetweenBytesTimeout", view)).toBe(9n);
  expect(guest.sync("request-options", "setFirstByteTimeout", view, 1n)).toEqual({
    tag: "err",
    val: { tag: "immutable" },
  });
  expect(
    guest.sync(
      "request-options",
      "setFirstByteTimeout",
      guest.sync("request-options", "clone", view),
      1n,
    ).tag,
  ).toBe("ok");
  const bare = guest.request("/").request;
  expect(guest.sync("request", "getOptions", bare)).toBeUndefined();
  await retire(host);
});

test("request and response accessors follow upstream parsing and defaults", async () => {
  const { host, guest } = standard();
  const trailers = guest.futureNew(TRAILERS);
  const [request] = guest.sync("request", "new", guest.fields([]), undefined, trailers, undefined);
  expect(guest.sync("request", "getMethod", request)).toEqual({ tag: "get" });
  expect(guest.sync("request", "getScheme", request)).toBeUndefined();
  expect(guest.sync("request", "getAuthority", request)).toBeUndefined();
  expect(guest.sync("request", "getPathWithQuery", request)).toBeUndefined();
  expect(guest.sync("request", "setMethod", request, { tag: "other", val: "PATCH" }).tag).toBe(
    "ok",
  );
  expect(guest.sync("request", "getMethod", request)).toEqual({ tag: "patch" });
  expect(guest.sync("request", "setMethod", request, { tag: "other", val: "PURGE" }).tag).toBe(
    "ok",
  );
  expect(guest.sync("request", "getMethod", request)).toEqual({ tag: "other", val: "PURGE" });
  expect(guest.sync("request", "setMethod", request, { tag: "other", val: "BAD METHOD" }).tag).toBe(
    "err",
  );
  expect(guest.sync("request", "setScheme", request, { tag: "other", val: "HTTPS" }).tag).toBe(
    "ok",
  );
  expect(guest.sync("request", "getScheme", request)).toEqual({ tag: "HTTPS" });
  expect(guest.sync("request", "setScheme", request, { tag: "other", val: "1x" }).tag).toBe("err");
  for (const value of ["user@example.test", "example.test:99999", "a:b:c", "exa mple.test", ""])
    expect(guest.sync("request", "setAuthority", request, value).tag).toBe("err");
  for (const value of ["example.test", "example.test:8080", "[::1]:8443", "127.0.0.1"])
    expect(guest.sync("request", "setAuthority", request, value).tag).toBe("ok");
  for (const value of ["relative", "", "/a b", "/\u0000"])
    expect(guest.sync("request", "setPathWithQuery", request, value).tag).toBe("err");
  expect(guest.sync("request", "setPathWithQuery", request, "/a?b=c#fragment").tag).toBe("ok");
  expect(guest.sync("request", "getPathWithQuery", request)).toBe("/a?b=c");
  expect(guest.sync("request", "setPathWithQuery", request, undefined).tag).toBe("ok");

  // A guest-built response: status rules and a consumable guest body with owned trailers.
  const contents = guest.streamNew();
  const responseTrailers = guest.futureNew(TRAILERS);
  const [response, transmitted] = guest.sync(
    "response",
    "new",
    guest.fields([["content-type", "text/plain"]]),
    contents,
    responseTrailers,
  );
  expect(guest.sync("response", "getStatusCode", response)).toBe(200);
  expect(guest.sync("response", "setStatusCode", response, 99).tag).toBe("err");
  expect(guest.sync("response", "setStatusCode", response, 600).tag).toBe("err");
  expect(guest.sync("response", "setStatusCode", response, 404).tag).toBe("ok");
  expect(guest.sync("response", "getStatusCode", response)).toBe(404);
  const res = guest.futureNew(RES);
  const [stream, trailersOut] = guest.sync("response", "consumeBody", response, res);
  expect(() => guest.sync("response", "getStatusCode", response)).toThrow("unknown or dropped");
  const writing = guest.streamWrite(contents, encoder.encode("body"));
  expect(decoder.decode(await guest.streamRead(stream))).toBe("body");
  await writing;
  guest.drop(contents);
  expect(await guest.streamRead(stream)).toBeUndefined();
  guest.futureWrite(responseTrailers, { tag: "ok", val: guest.fields([["x-trailer", "t"]]) });
  const delivered = await guest.futureRead(trailersOut, OUT_TRAILERS);
  expect(delivered.tag).toBe("ok");
  expect(text(guest.sync("fields", "copyAll", delivered.val))).toEqual([["x-trailer", "t"]]);
  // The consumer's `res` outcome becomes the transmission result of `new`.
  guest.futureWrite(res, { tag: "err", val: { tag: "HTTP-protocol-error" } });
  expect(await guest.futureRead(transmitted, RESULT)).toEqual({
    tag: "err",
    val: { tag: "HTTP-protocol-error" },
  });
  await retire(host);
});

test("a refused request.new transfers none of its owned, stream or future claims", async () => {
  const { host, guest } = standard();
  const headers = guest.fields([["x-a", "1"]]);
  const contents = guest.streamNew();
  const trailers = guest.futureNew(TRAILERS);
  const options = guest.sync("request-options", "constructor");
  guest.drop(options);
  const before = host.size;
  expect(() => guest.sync("request", "new", headers, contents, trailers, options)).toThrow(
    "unknown or dropped handle",
  );
  // A handle of another resource type is refused the same way.
  expect(() => guest.sync("request", "new", headers, contents, trailers, headers)).toThrow();
  expect(host.size).toBe(before);
  expect(guest.sync("fields", "get", headers, "x-a")).toEqual([encoder.encode("1")]);
  const [request] = guest.sync("request", "new", headers, contents, trailers, undefined);
  expect(() => guest.sync("fields", "get", headers, "x-a")).toThrow("unknown or dropped");
  // The stream and future readable ends moved once; a second transfer is refused.
  expect(() =>
    guest.sync("request", "new", guest.fields([]), contents, guest.futureNew(TRAILERS), undefined),
  ).toThrow("already transferred");
  expect(() =>
    guest.sync("request", "new", guest.fields([]), undefined, trailers, undefined),
  ).toThrow("already transferred");
  guest.drop(request);
  await retire(host);
});

test("send checks the exact grant before the provider and refuses noncanonical targets", async () => {
  let calls = 0;
  const { host, guest } = standard(async () => {
    calls++;
    return new Response(null);
  });
  const signal = new AbortController().signal;
  for (const [authority, scheme, code] of [
    ["other.example.test", { tag: "HTTPS" }, "HTTP-request-denied"],
    ["api.example.test:8443", { tag: "HTTPS" }, "HTTP-request-denied"],
    ["api.example.test", { tag: "HTTP" }, "HTTP-request-denied"],
    ["127.1", { tag: "HTTPS" }, "HTTP-request-denied"],
    ["api.example.test:0", { tag: "HTTPS" }, "HTTP-request-denied"],
    ["api.example.test", { tag: "other", val: "ftp" }, "HTTP-protocol-error"],
  ] as const) {
    const { request, result } = guest.request("/", { authority });
    expect(guest.sync("request", "setScheme", request, scheme).tag).toBe("ok");
    expect(await guest.send(request, signal)).toEqual({ tag: "err", val: { tag: code } });
    // The transmission result reports the same refusal.
    expect(await guest.futureRead(result, RESULT)).toEqual({ tag: "err", val: { tag: code } });
  }
  const { request } = guest.request("/");
  expect(guest.sync("request", "setPathWithQuery", request, undefined).tag).toBe("ok");
  expect(await guest.send(request, signal)).toEqual({
    tag: "err",
    val: { tag: "HTTP-request-URI-invalid" },
  });
  expect(calls).toBe(0);
  // No scheme defaults to HTTPS, and an uppercase authority is the same canonical origin.
  const defaulted = guest.request("/", { authority: "API.example.test" }).request;
  expect(guest.sync("request", "setScheme", defaulted, undefined).tag).toBe("ok");
  expect((await guest.send(defaulted, signal)).tag).toBe("ok");
  expect(calls).toBe(1);
  // Without any provider the same origin is denied.
  const bare = standard(undefined);
  expect(await bare.guest.send(bare.guest.request("/").request, signal)).toEqual({
    tag: "err",
    val: { tag: "HTTP-request-denied" },
  });
  await retire(bare.host);
  await retire(host);
});

test("HTTPS Fetch: raw request fields and body, raw response fields, SSE first chunk before EOF", async () => {
  let source!: ReadableStreamDefaultController<Uint8Array>;
  let seen: { url: string; init: RequestInit } | undefined;
  const { host, guest } = standard(async (input, init) => {
    seen = { url: String(input), init: init! };
    return new Response(
      new ReadableStream<Uint8Array>({
        start(controller) {
          source = controller;
        },
      }),
      {
        status: 201,
        headers: [
          ["content-type", "text/event-stream"],
          ["x-raw", "ÿ\u0080A"],
          ["connection", "keep-alive"],
        ],
      },
    );
  });
  const callback = new AbortController();
  const contents = guest.streamNew();
  const trailers = guest.futureNew(TRAILERS);
  const upload = Uint8Array.from({ length: 150_000 }, (_, at) => (at * 7) % 256);
  const { request, result } = guest.request("/upload?q=1", {
    headers: [
      ["x-dup", "one"],
      ["x-dup", "two"],
      ["x-bytes", [0xff, 0x41]],
      ["content-length", String(upload.length)],
    ],
    contents,
    trailers,
    method: { tag: "post" },
  });
  const sending = guest.send(request, callback.signal);
  await guest.streamWrite(contents, upload, callback.signal);
  guest.drop(contents);
  expect(seen).toBeUndefined();
  guest.futureWrite(trailers, { tag: "ok" });
  const sent = await sending;
  expect(sent.tag).toBe("ok");
  expect(seen!.url).toBe(`${ORIGIN}/upload?q=1`);
  expect(seen!.init.method).toBe("POST");
  expect(seen!.init.redirect).toBe("manual");
  expect(seen!.init.credentials).toBe("omit");
  const headers = seen!.init.headers as Headers;
  expect(headers.get("x-dup")).toBe("one, two");
  expect(headers.get("x-bytes")).toBe("ÿA");
  expect(seen!.init.body).toEqual(upload);
  expect(await guest.futureRead(result, RESULT)).toEqual({ tag: "ok" });

  const response = sent.val;
  expect(guest.sync("response", "getStatusCode", response)).toBe(201);
  const fields = guest.sync("response", "getHeaders", response);
  expect(guest.sync("fields", "get", fields, "x-raw")).toEqual([Uint8Array.of(0xff, 0x80, 0x41)]);
  // Upstream strips forbidden fields before they reach the guest.
  expect(guest.sync("fields", "has", fields, "connection")).toBe(false);
  const [stream, trailersOut] = guest.sync(
    "response",
    "consumeBody",
    response,
    guest.futureNew(RES),
  );
  const first = guest.streamRead(stream, callback.signal);
  source.enqueue(encoder.encode("data: one\n\n"));
  expect(decoder.decode(await first)).toBe("data: one\n\n");
  source.close();
  expect(await guest.streamRead(stream, callback.signal)).toBeUndefined();
  // Fetch exposes no trailers.
  expect(await guest.futureRead(trailersOut, OUT_TRAILERS)).toEqual({ tag: "ok" });
  await retire(host);
});

test("a published response is revision-owned across callbacks; idle bodies never block a callback", async () => {
  let source!: ReadableStreamDefaultController<Uint8Array>;
  const cancelled = deferred();
  const ack = deferred();
  const { host, guest } = standard(
    async () =>
      new Response(
        new ReadableStream<Uint8Array>({
          start(controller) {
            source = controller;
          },
          cancel() {
            cancelled.resolve();
            return ack.promise;
          },
        }),
      ),
  );
  const first = new AbortController();
  const response = (await guest.send(guest.request("/sse").request, first.signal)).val;
  const [stream] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  const pending = guest.streamRead(stream, first.signal);
  await tick();
  // The callback ends with a read in flight on an idle body: it settles without the body.
  first.abort();
  await expect(pending).rejects.toThrow("cancelled");
  host.endCallback();
  await host.settled();
  // The next callback continues the same body; the in-flight read's data is not lost.
  const second = new AbortController();
  source.enqueue(encoder.encode("data: two\n\n"));
  expect(decoder.decode(await guest.streamRead(stream, second.signal))).toBe("data: two\n\n");
  // Dropping the body cancels it, and the cancellation promise is the physical ACK.
  guest.drop(stream);
  await cancelled.promise;
  let settled = false;
  const settling = host.settled().then(() => {
    settled = true;
  });
  await tick();
  expect(settled).toBe(false);
  expect(host.pending).toBeGreaterThan(0);
  ack.resolve();
  await settling;
  await retire(host);
});

test("cancellation before headers aborts Fetch and tracks the late provider promise to its ACK", async () => {
  const provided = deferred<Response>();
  const cancelled = deferred();
  const ack = deferred();
  let aborted: AbortSignal | undefined;
  const { host, guest } = standard((_input, init) => {
    aborted = init!.signal!;
    return provided.promise;
  });
  const callback = new AbortController();
  const { request, result } = guest.request("/slow");
  const sending = guest.send(request, callback.signal);
  await tick();
  expect(aborted?.aborted).toBe(false);
  callback.abort();
  await expect(sending).rejects.toThrow("cancelled");
  expect(aborted?.aborted).toBe(true);
  expect(await guest.futureRead(result, RESULT, new AbortController().signal)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "request was cancelled" },
  });
  let settled = false;
  const settling = host.settled().then(() => {
    settled = true;
  });
  await tick();
  // The provider ignored the abort: its promise is still physical work of the revision.
  expect(settled).toBe(false);
  provided.resolve(
    new Response(
      new ReadableStream({
        cancel() {
          cancelled.resolve();
          return ack.promise;
        },
      }),
    ),
  );
  await cancelled.promise;
  await tick();
  expect(settled).toBe(false);
  ack.resolve();
  await settling;
  await retire(host);
});

test("uploads commit only complete bodies: abandoned, oversized and mismatched uploads never reach Fetch", async () => {
  let calls = 0;
  const { host, guest } = standard(async () => {
    calls++;
    return new Response(null);
  });
  // The callback ends with the trailers future unwritten: the upload fails, never truncated.
  const first = new AbortController();
  const contents = guest.streamNew();
  const { request, result } = guest.request("/", {
    contents,
    method: { tag: "post" },
  });
  const sending = guest.send(request, first.signal);
  await guest.streamWrite(contents, encoder.encode("partial"), first.signal);
  host.endCallback();
  expect(await sending).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "callback ended" },
  });
  expect(await guest.futureRead(result, RESULT)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "callback ended" },
  });
  // A declared Content-Length must match exactly.
  for (const declared of ["3", "100"]) {
    const body = guest.streamNew();
    const trailers = guest.futureNew(TRAILERS);
    const short = guest.request("/", {
      headers: [["content-length", declared]],
      contents: body,
      trailers,
      method: { tag: "put" },
    });
    const pending = guest.send(short.request, new AbortController().signal);
    await guest.streamWrite(body, encoder.encode("hello"));
    guest.drop(body);
    guest.futureWrite(trailers, { tag: "ok" });
    expect(await pending).toEqual({ tag: "err", val: { tag: "HTTP-request-body-size", val: 5n } });
  }
  // A guest-reported trailers error fails the upload with that code.
  const failing = guest.futureNew(TRAILERS);
  const errored = guest.request("/", { contents: guest.streamNew(), trailers: failing });
  const erroring = guest.send(errored.request, new AbortController().signal);
  guest.futureWrite(failing, { tag: "err", val: { tag: "HTTP-request-trailer-section-size" } });
  host.endCallback();
  expect(await erroring).toEqual({
    tag: "err",
    val: { tag: "HTTP-request-trailer-section-size" },
  });
  expect(calls).toBe(0);
  await retire(host);
});

test("browser platform limits are honest standard errors before any provider call", async () => {
  let calls = 0;
  const { host, guest } = standard(async () => {
    calls++;
    return new Response(null);
  });
  const signal = new AbortController().signal;
  // Fetch cannot send request trailers.
  const written = guest.futureNew(TRAILERS);
  const trailered = guest.request("/", { trailers: written });
  guest.futureWrite(written, { tag: "ok", val: guest.fields([["x-checksum", "1"]]) });
  expect(await guest.send(trailered.request, signal)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "browser Fetch cannot send request trailers" },
  });
  // Empty trailers are the same as none.
  const empty = guest.futureNew(TRAILERS);
  const emptied = guest.request("/", { trailers: empty });
  guest.futureWrite(empty, { tag: "ok", val: guest.sync("fields", "constructor") });
  expect((await guest.send(emptied.request, signal)).tag).toBe("ok");
  // Fetch cannot send a body with GET, nor the CONNECT/TRACE/TRACK methods.
  const body = guest.streamNew();
  const done = guest.futureNew(TRAILERS);
  const get = guest.request("/", { contents: body, trailers: done });
  const getting = guest.send(get.request, signal);
  await guest.streamWrite(body, encoder.encode("x"));
  guest.drop(body);
  guest.futureWrite(done, { tag: "ok" });
  expect(await getting).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "browser Fetch cannot send a body with GET" },
  });
  for (const name of ["connect", "trace"]) {
    const { request } = guest.request("/", { method: { tag: name } });
    expect(await guest.send(request, signal)).toEqual({
      tag: "err",
      val: { tag: "HTTP-request-method-invalid" },
    });
  }
  expect(calls).toBe(1);
  await retire(host);
});

test("redirects are never followed; provider codes, timeouts and failures map to error codes", async () => {
  let cancels = 0;
  let next: (init: RequestInit) => Promise<Response>;
  const { host, guest } = standard((_input, init) => next(init!));
  const signal = new AbortController().signal;
  const fake = (properties: Partial<Response>) =>
    ({
      type: "basic",
      redirected: false,
      status: 302,
      headers: new Headers(),
      body: new ReadableStream({
        cancel() {
          cancels++;
        },
      }),
      ...properties,
    }) as Response;
  next = async () => fake({ type: "opaqueredirect", status: 0 });
  expect(await guest.send(guest.request("/").request, signal)).toEqual({
    tag: "err",
    val: {
      tag: "internal-error",
      val: "browser Fetch exposes redirect responses only as opaque responses",
    },
  });
  next = async () => fake({ redirected: true, status: 200 });
  expect((await guest.send(guest.request("/").request, signal)).val.val).toBe(
    "the Fetch implementation followed a redirect",
  );
  await host.settled();
  expect(cancels).toBe(2);
  // A non-browser provider's real 3xx passes through, as native does.
  next = async () => new Response(null, { status: 302, headers: { location: "/elsewhere" } });
  const redirect = (await guest.send(guest.request("/").request, signal)).val;
  expect(guest.sync("response", "getStatusCode", redirect)).toBe(302);
  next = async () => {
    throw new HttpProviderError({ tag: "DNS-timeout" });
  };
  expect(await guest.send(guest.request("/").request, signal)).toEqual({
    tag: "err",
    val: { tag: "DNS-timeout" },
  });
  next = async () => {
    throw new HttpProviderError({ tag: "no-such-code" } as never);
  };
  expect((await guest.send(guest.request("/").request, signal)).val.tag).toBe("internal-error");
  next = async () => {
    throw new TypeError("Failed to fetch");
  };
  expect(await guest.send(guest.request("/").request, signal)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "Failed to fetch" },
  });
  // first-byte-timeout bounds the wait for response headers.
  next = (init) =>
    new Promise((_, reject) =>
      init.signal!.addEventListener("abort", () =>
        reject(new DOMException("aborted", "AbortError")),
      ),
    );
  const options = guest.sync("request-options", "constructor");
  expect(guest.sync("request-options", "setFirstByteTimeout", options, 1_000_000n).tag).toBe("ok");
  expect(await guest.send(guest.request("/", { options }).request, signal)).toEqual({
    tag: "err",
    val: { tag: "connection-read-timeout" },
  });
  // between-bytes-timeout bounds each body read; the trailers future reports it.
  next = async () => new Response(new ReadableStream({}));
  const quiet = guest.sync("request-options", "constructor");
  expect(guest.sync("request-options", "setBetweenBytesTimeout", quiet, 1_000_000n).tag).toBe("ok");
  const response = (await guest.send(guest.request("/", { options: quiet }).request, signal)).val;
  const [stream, trailersOut] = guest.sync(
    "response",
    "consumeBody",
    response,
    guest.futureNew(RES),
  );
  expect(await guest.streamRead(stream, signal)).toBeUndefined();
  expect(await guest.futureRead(trailersOut, OUT_TRAILERS)).toEqual({
    tag: "err",
    val: { tag: "connection-read-timeout" },
  });
  await retire(host);
});

test("retirement cancels retained responses and unsent requests and leaves zero resources", async () => {
  const cancelled = deferred();
  const { host, guest } = standard(
    async () =>
      new Response(
        new ReadableStream({
          cancel() {
            cancelled.resolve();
          },
        }),
      ),
  );
  const signal = new AbortController().signal;
  const retained = (await guest.send(guest.request("/").request, signal)).val;
  expect(guest.sync("response", "getStatusCode", retained)).toBe(200);
  // An unsent request keeps its accepted upload bytes until retirement releases them.
  const contents = guest.streamNew();
  guest.request("/", { contents });
  await guest.streamWrite(contents, new Uint8Array(1 << 16));
  expect(host.inputBytes).toBe(1 << 16);
  await retire(host);
  await cancelled.promise;
});

test("the Fetch adapter buffers at most FETCH_BODY_BYTES and never calls Fetch beyond it", async () => {
  let calls = 0;
  const { host, guest } = standard(async () => {
    calls++;
    return new Response(null);
  });
  const signal = new AbortController().signal;
  const contents = guest.streamNew();
  const { request, result } = guest.request("/", { contents, method: { tag: "post" } });
  const sending = guest.send(request, signal);
  // The excess chunk is refused once the cap is crossed; later writes see the stopped reader.
  await guest
    .streamWrite(contents, new Uint8Array(FETCH_BODY_BYTES + 65_536), signal)
    .catch(() => {});
  const code = { tag: "HTTP-request-body-size", val: BigInt(FETCH_BODY_BYTES + 65_536) };
  expect(await sending).toEqual({ tag: "err", val: code });
  expect(await guest.futureRead(result, RESULT)).toEqual({ tag: "err", val: code });
  expect(calls).toBe(0);
  await retire(host);
});
