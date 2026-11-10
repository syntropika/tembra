// Regressions for PHASE4A_BROWSER_REVIEW.md (H1–H4, M1–M3, L1, L2): the exchange lifetime the
// browser host owns around a raw `HttpProvider`, whatever the provider does. Local providers only.
import { expect, test } from "bun:test";
import { BrowserNetwork } from "../src/interpreter/browser-network.ts";
import type {
  HttpField,
  HttpProvider,
  HttpProviderRequest,
  HttpProviderResponse,
} from "../src/interpreter/http-provider.ts";
import {
  decoder,
  deferred,
  encoder,
  OUT_TRAILERS,
  RES,
  RESULT,
  retire,
  standard,
  tick,
  TRAILERS,
} from "./standard-guest.ts";

const CHUNK = 65_536;
const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** A provider answering every request with `answer`, recording what it was sent. */
function canned(
  answer: (request: HttpProviderRequest) => HttpProviderResponse | Promise<HttpProviderResponse>,
) {
  const requests: HttpProviderRequest[] = [];
  const provider: HttpProvider = {
    async send(request) {
      requests.push(request);
      return answer(request);
    },
  };
  return { provider, requests };
}

/** A response body the test feeds by hand; `cancelled` resolves when the host cancels it. */
function feed(ack: Promise<void> = Promise.resolve()) {
  let controller!: ReadableStreamDefaultController<Uint8Array>;
  const cancelled = deferred();
  const body = new ReadableStream<Uint8Array>({
    start(started) {
      controller = started;
    },
    cancel() {
      cancelled.resolve();
      return ack;
    },
  });
  return { body, controller, cancelled: cancelled.promise };
}

/** Read a guest output stream to its end. */
async function drain(guest: ReturnType<typeof standard>["guest"], stream: number) {
  const chunks: string[] = [];
  for (let chunk = await guest.streamRead(stream); chunk; chunk = await guest.streamRead(stream))
    chunks.push(decoder.decode(chunk));
  return chunks;
}

test("H1: a provider that ignores request bodies cannot exhaust the revision input budget", async () => {
  const { provider, requests } = canned(() => ({ status: 413, fields: [], body: null }));
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;
  for (let post = 0; post < 16; post++) {
    const contents = guest.streamNew();
    const trailers = guest.futureNew(TRAILERS);
    const { request, result } = guest.request("/upload", {
      contents,
      trailers,
      method: { tag: "post" },
    });
    const response = (await guest.send(request, signal)).val;
    await guest.streamWrite(contents, new Uint8Array(CHUNK), signal);
    expect(host.inputBytes).toBe(CHUNK);
    // An early final response, released unread: the host ends the upload itself.
    guest.drop(response);
    expect(host.inputBytes).toBe(0);
    expect(requests[post]!.signal.aborted).toBe(true);
    await expect(requests[post]!.trailers).rejects.toThrow();
    expect(await guest.futureRead(result, RESULT)).toEqual({
      tag: "err",
      val: {
        tag: "internal-error",
        val: "the response was released before its request body completed",
      },
    });
    await expect(guest.streamWrite(contents, new Uint8Array(1), signal)).rejects.toThrow(
      "stream reader was dropped",
    );
    guest.drop(contents);
    guest.futureWrite(trailers, { tag: "ok" });
  }
  // An unrelated stream of the same revision still makes progress.
  const other = guest.streamNew();
  await guest.streamWrite(other, new Uint8Array(CHUNK), signal);
  guest.drop(other);
  await retire(host);
});

test("H1: retirement discards an upload whose provider never reads it, and errors its body", async () => {
  const { provider, requests } = canned(() => ({ status: 200, fields: [], body: null }));
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;
  const contents = guest.streamNew();
  const { request } = guest.request("/upload", { contents, method: { tag: "post" } });
  const response = (await guest.send(request, signal)).val;
  await guest.streamWrite(contents, new Uint8Array(CHUNK), signal);
  // A clean, empty response body ends nothing else: the upload may still be consumed.
  const [stream] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  expect(await guest.streamRead(stream)).toBeUndefined();
  host.endCallback();
  await host.settled();
  expect(requests[0]!.signal.aborted).toBe(false);
  expect(host.inputBytes).toBe(CHUNK);
  await host.dispose();
  expect(host.inputBytes).toBe(0);
  await expect(requests[0]!.body!.getReader().read()).rejects.toMatchObject({
    code: { tag: "internal-error", val: "the request body was discarded" },
  });
  await retire(host);
});

test("H2: an empty response chunk is not the end of the body", async () => {
  const { provider } = canned(() => {
    const { body, controller } = feed();
    for (const text of ["a", "", "b"]) controller.enqueue(encoder.encode(text));
    controller.close();
    return {
      status: 200,
      fields: [],
      body,
      trailers: Promise.resolve([["x-t", encoder.encode("1")]] as HttpField[]),
    };
  });
  const { host, guest } = standard(provider);
  const response = (await guest.send(guest.request("/").request, new AbortController().signal)).val;
  const [stream, trailers] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  expect((await drain(guest, stream)).join("")).toBe("ab");
  const delivered = await guest.futureRead(trailers, OUT_TRAILERS);
  expect(guest.sync("fields", "get", delivered.val, "x-t")).toEqual([encoder.encode("1")]);
  await retire(host);
});

test("H3: dropping the body early reports no trailers even while the provider's never settle", async () => {
  const ack = deferred();
  const fed = feed(ack.promise);
  const { provider, requests } = canned(() => ({
    status: 200,
    fields: [],
    body: fed.body,
    trailers: new Promise<never>(() => {}),
  }));
  const { host, guest } = standard(provider);
  const response = (await guest.send(guest.request("/").request, new AbortController().signal)).val;
  const [stream, trailers] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  fed.controller.enqueue(encoder.encode("partial"));
  expect(decoder.decode(await guest.streamRead(stream))).toBe("partial");
  guest.drop(stream);
  expect(await guest.futureRead(trailers, OUT_TRAILERS)).toEqual({ tag: "ok" });
  expect(requests[0]!.signal.aborted).toBe(true);
  await fed.cancelled;
  let settled = false;
  const settling = host.settled().then(() => {
    settled = true;
  });
  await tick();
  expect(settled).toBe(false);
  ack.resolve();
  await settling;
  await retire(host);
});

test("H4: a clean end of the body waits for trailers and lets a duplex upload finish", async () => {
  const fed = feed();
  const trailers = deferred<readonly HttpField[] | undefined>();
  const { provider, requests } = canned((request) => {
    // A transport that fails its trailers if the host aborts the exchange.
    request.signal.addEventListener("abort", () => trailers.resolve(undefined));
    return { status: 200, fields: [], body: fed.body, trailers: trailers.promise };
  });
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;
  const contents = guest.streamNew();
  const upload = guest.futureNew(TRAILERS);
  const { request, result } = guest.request("/duplex", {
    contents,
    trailers: upload,
    method: { tag: "post" },
  });
  const response = (await guest.send(request, signal)).val;
  const [stream, out] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  fed.controller.enqueue(encoder.encode("done"));
  fed.controller.close();
  expect(await drain(guest, stream)).toEqual(["done"]);
  guest.drop(stream);
  expect(requests[0]!.signal.aborted).toBe(false);
  // The response trailers arrive after its end; the guest reads them as owned fields.
  const reading = guest.futureRead(out, OUT_TRAILERS);
  await tick();
  trailers.resolve([["x-late", encoder.encode("yes")]]);
  const delivered = await reading;
  expect(guest.sync("fields", "get", delivered.val, "x-late")).toEqual([encoder.encode("yes")]);
  // The upload, still running after the response ended, completes normally.
  const reader = requests[0]!.body!.getReader();
  const writing = guest.streamWrite(contents, encoder.encode("late upload"), signal);
  expect(decoder.decode((await reader.read()).value)).toBe("late upload");
  await writing;
  guest.drop(contents);
  guest.futureWrite(upload, { tag: "ok" });
  expect((await reader.read()).done).toBe(true);
  expect(await guest.futureRead(result, RESULT)).toEqual({ tag: "ok" });
  await retire(host);
});

test("M1: the host enforces both timeouts for a provider that ignores its signal", async () => {
  const late = deferred<HttpProviderResponse>();
  const ack = deferred();
  const fed = feed(ack.promise);
  let calls = 0;
  const provider: HttpProvider = {
    send: () =>
      calls++ === 0 ? late.promise : Promise.resolve({ status: 200, fields: [], body: fed.body }),
  };
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;
  const options = guest.sync("request-options", "constructor");
  guest.sync("request-options", "setFirstByteTimeout", options, 10_000_000n);
  const started = Date.now();
  expect(await guest.send(guest.request("/", { options }).request, signal)).toEqual({
    tag: "err",
    val: { tag: "connection-read-timeout" },
  });
  expect(Date.now() - started).toBeLessThan(1000);
  // The late response is cancelled to its acknowledgement, never published.
  const lateBody = feed();
  late.resolve({ status: 200, fields: [], body: lateBody.body });
  await lateBody.cancelled;
  // Between bytes: a body that never produces again fails its read and trailers.
  const quiet = guest.sync("request-options", "constructor");
  guest.sync("request-options", "setBetweenBytesTimeout", quiet, 10_000_000n);
  const response = (await guest.send(guest.request("/", { options: quiet }).request, signal)).val;
  const [stream, out] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  expect(await guest.streamRead(stream)).toBeUndefined();
  expect(await guest.futureRead(out, OUT_TRAILERS)).toEqual({
    tag: "err",
    val: { tag: "connection-read-timeout" },
  });
  await fed.cancelled;
  ack.resolve();
  await retire(host);
});

test("M2: a raw provider receives the guest's exact path and query", async () => {
  const { provider, requests } = canned(() => ({ status: 204, fields: [], body: null }));
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;
  const paths = ["/a/../b", "/a/%2e%2e/b", "/x\\y", "/q?a='b'", "/sp{x}?s='1'"];
  for (const path of paths) await guest.send(guest.request(path).request, signal);
  expect(requests.map((request) => request.pathWithQuery)).toEqual(paths);
  // The URL still names the granted origin; only Fetch-style transports use its path.
  expect(new Set(requests.map((request) => request.url.origin))).toEqual(
    new Set(["https://api.example.test"]),
  );
  expect(requests[0]!.url.pathname).toBe("/b");
  await retire(host);
});

test("M3: the host enforces a response Content-Length except where HTTP has no body", async () => {
  const cases: [string, number, string, string[], unknown][] = [
    ["GET", 200, "100", ["short"], { tag: "err", val: { tag: "HTTP-response-incomplete" } }],
    [
      "GET",
      200,
      "3",
      ["toolong"],
      { tag: "err", val: { tag: "HTTP-response-body-size", val: 7n } },
    ],
    ["GET", 200, "5", ["ab", "", "cde"], { tag: "ok" }],
    ["HEAD", 200, "100", [], { tag: "ok" }],
    ["GET", 204, "100", [], { tag: "ok" }],
    ["GET", 304, "100", [], { tag: "ok" }],
  ];
  for (const [method, status, length, chunks, expected] of cases) {
    const fed = feed();
    for (const chunk of chunks) fed.controller.enqueue(encoder.encode(chunk));
    fed.controller.close();
    const { provider } = canned(() => ({
      status,
      fields: [["content-length", encoder.encode(length)]],
      body: fed.body,
    }));
    const { host, guest } = standard(provider);
    const { request } = guest.request("/", { method: { tag: method.toLowerCase() } });
    const response = (await guest.send(request, new AbortController().signal)).val;
    const [stream, out] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
    const read = await drain(guest, stream);
    // A short or overlong body never ends cleanly; overlong bytes are never delivered.
    expect(read.join("").length).toBeLessThanOrEqual(Number(length));
    expect(await guest.futureRead(out, OUT_TRAILERS)).toEqual(expected);
    await retire(host);
  }
  const conflicting = canned(() => ({
    status: 200,
    fields: [
      ["content-length", encoder.encode("1")],
      ["content-length", encoder.encode("2")],
    ],
    body: null,
  }));
  const { host, guest } = standard(conflicting.provider);
  expect(await guest.send(guest.request("/").request, new AbortController().signal)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "the HTTP provider returned conflicting lengths" },
  });
  await retire(host);
});

test("L1: the provider gets copies of field and trailer bytes", async () => {
  const { provider, requests } = canned(async (request) => {
    for (const [, value] of request.fields) value.fill(0x5a);
    for (const [, value] of (await request.trailers) ?? []) value.fill(0x5a);
    return { status: 200, fields: [], body: null };
  });
  const { host, guest } = standard(provider);
  const trailers = guest.futureNew(TRAILERS);
  const { request } = guest.request("/", { headers: [["x-a", "abc"]], trailers });
  const view = guest.sync("request", "getHeaders", request);
  const fields = guest.fields([["x-t", "def"]]);
  guest.futureWrite(trailers, { tag: "ok", val: fields });
  expect((await guest.send(request, new AbortController().signal)).tag).toBe("ok");
  expect(requests[0]!.fields).toEqual([["x-a", Uint8Array.of(0x5a, 0x5a, 0x5a)]]);
  expect(guest.sync("fields", "get", view, "x-a")).toEqual([encoder.encode("abc")]);
  await retire(host);
});

test("L2: targets follow the native host rules, and grants refuse port 0", async () => {
  const { provider, requests } = canned(() => ({ status: 204, fields: [], body: null }));
  const { host, guest } = standard(provider, ["https://api.example.test", "https://[::1]"]);
  const signal = new AbortController().signal;
  const accepted = ["[0:0::1]", "api.example.test:0443", "API.Example.Test:443"];
  for (const authority of accepted)
    expect((await guest.send(guest.request("/", { authority }).request, signal)).tag).toBe("ok");
  expect(requests.map((request) => request.url.origin)).toEqual([
    "https://[::1]",
    "https://api.example.test",
    "https://api.example.test",
  ]);
  for (const authority of ["api.example.test:0", "127.1", "0x7f.1", "01.2.3.4", "a.-b.test"])
    expect(await guest.send(guest.request("/", { authority }).request, signal)).toEqual({
      tag: "err",
      val: { tag: "HTTP-request-denied" },
    });
  expect(requests.length).toBe(accepted.length);
  expect(
    () =>
      new BrowserNetwork(
        { http: [{ provider: "p", origins: ["https://api.example.test:0"] }] },
        {},
      ),
  ).toThrow("port 0");
  await retire(host);
});

test("a slow consumer and retirement leave nothing behind", async () => {
  // Retirement during a pending provider read still waits for its actual cancel.
  const ack = deferred();
  const fed = feed(ack.promise);
  const { provider } = canned(() => ({ status: 200, fields: [], body: fed.body }));
  const { host, guest } = standard(provider);
  const response = (await guest.send(guest.request("/").request, new AbortController().signal)).val;
  const [stream] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  const callback = new AbortController();
  const reading = guest.streamRead(stream, callback.signal);
  await sleep(1);
  callback.abort();
  await expect(reading).rejects.toThrow("cancelled");
  host.endCallback();
  await host.settled();
  let disposed = false;
  const disposing = host.dispose().then(() => {
    disposed = true;
  });
  await fed.cancelled;
  await tick();
  expect(disposed).toBe(false);
  ack.resolve();
  await disposing;
  expect(host.size).toBe(0);
  expect(host.pending).toBe(0);
  expect(host.inputBytes).toBe(0);
});
