// The typed `HttpProvider` transport behind browser `wasi:http`: a raw provider receives the
// exact guest upload as a pull stream and returns raw fields, a body stream and trailers. These
// tests drive it with raw guest packets and local deterministic providers only.
import { expect, test } from "bun:test";
import { WRITE_CLOSED } from "../src/interpreter/standard-codec.ts";
import {
  type HttpField,
  type HttpProvider,
  HttpProviderError,
  type HttpProviderRequest,
  type HttpProviderResponse,
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

/** A provider whose requests the test observes and answers by hand. */
function manual() {
  const requests: HttpProviderRequest[] = [];
  const answers: ((response: HttpProviderResponse | Promise<never>) => void)[] = [];
  const provider: HttpProvider = {
    send(request) {
      requests.push(request);
      return new Promise((resolve) => answers.push(resolve));
    },
  };
  return { provider, requests, answers };
}

/** A body stream the test feeds; `cancelled` resolves when the host cancels it. */
function source(ack: Promise<void> = Promise.resolve()) {
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

const field = (name: string, ...bytes: number[]): HttpField => [name, Uint8Array.from(bytes)];

test("duplex: the provider pulls request chunks and the guest reads the response before upload EOF", async () => {
  const { provider, requests, answers } = manual();
  const { host, guest } = standard(provider);
  const callback = new AbortController();
  const contents = guest.streamNew();
  const trailers = guest.futureNew(TRAILERS);
  const { request, result } = guest.request("/duplex", {
    headers: [
      ["x-dup", [0xff, 0x80]],
      ["X-Dup", [0x41]],
    ],
    contents,
    trailers,
    method: { tag: "post" },
  });
  const sending = guest.send(request, callback.signal);
  await tick();
  const seen = requests[0]!;
  expect(seen.url.href).toBe("https://api.example.test/duplex");
  expect(seen.method).toBe("POST");
  // Raw duplicates stay separate, in order, as bytes.
  expect(seen.fields).toEqual([field("x-dup", 0xff, 0x80), field("x-dup", 0x41)]);
  const upload = seen.body!.getReader();
  const first = Uint8Array.from({ length: 70_000 }, (_, at) => at % 251);
  const writing = guest.streamWrite(contents, first, callback.signal);
  const one = await upload.read();
  const two = await upload.read();
  await writing;
  // Each pull is one accepted guest chunk: no body-wide copy, no read-ahead.
  expect(one.value!.byteLength).toBe(65_536);
  expect([...one.value!, ...two.value!]).toEqual([...first]);

  // The response arrives while the upload is still open; its first event is readable now.
  const body = source();
  const responseTrailers = deferred<readonly HttpField[] | undefined>();
  answers[0]!({
    status: 207,
    fields: [
      field("x-raw", 0xff, 0x80),
      ["set-cookie", encoder.encode("a=1")],
      ["set-cookie", encoder.encode("b=2")],
      ["connection", encoder.encode("close")],
    ],
    body: body.body,
    trailers: responseTrailers.promise,
  });
  const sent = await sending;
  expect(sent.tag).toBe("ok");
  const response = sent.val;
  expect(guest.sync("response", "getStatusCode", response)).toBe(207);
  const headers = guest.sync("response", "getHeaders", response);
  expect(guest.sync("fields", "get", headers, "set-cookie")).toEqual([
    encoder.encode("a=1"),
    encoder.encode("b=2"),
  ]);
  expect(guest.sync("fields", "get", headers, "x-raw")).toEqual([Uint8Array.of(0xff, 0x80)]);
  expect(guest.sync("fields", "has", headers, "connection")).toBe(false);
  const [stream, trailersOut] = guest.sync(
    "response",
    "consumeBody",
    response,
    guest.futureNew(RES),
  );
  const event = guest.streamRead(stream, callback.signal);
  body.controller.enqueue(encoder.encode("data: one\n\n"));
  expect(decoder.decode(await event)).toBe("data: one\n\n");

  // The rest of the upload and its trailers follow the response.
  const last = guest.streamWrite(contents, encoder.encode("tail"), callback.signal);
  expect(decoder.decode((await upload.read()).value)).toBe("tail");
  await last;
  guest.drop(contents);
  let ended = false;
  const end = upload.read().then((next) => {
    ended = next.done;
  });
  await tick();
  // No clean end before the trailers result.
  expect(ended).toBe(false);
  guest.futureWrite(trailers, {
    tag: "ok",
    val: guest.fields([
      ["x-sum", [0xfe]],
      ["x-sum", "2"],
    ]),
  });
  await end;
  expect(ended).toBe(true);
  expect(await seen.trailers).toEqual([field("x-sum", 0xfe), ["x-sum", encoder.encode("2")]]);
  expect(await guest.futureRead(result, RESULT)).toEqual({ tag: "ok" });

  // Response trailers arrive after the body as owned guest fields with raw duplicates.
  body.controller.close();
  expect(await guest.streamRead(stream, callback.signal)).toBeUndefined();
  responseTrailers.resolve([field("x-check", 0xff), field("x-check", 0x41)]);
  const delivered = await guest.futureRead(trailersOut, OUT_TRAILERS);
  expect(delivered.tag).toBe("ok");
  expect(guest.sync("fields", "get", delivered.val, "x-check")).toEqual([
    Uint8Array.of(0xff),
    Uint8Array.of(0x41),
  ]);
  await retire(host);
});

test("a failed upload errors the provider's body and never closes it cleanly", async () => {
  const { provider, requests, answers } = manual();
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;

  // Abandoned trailers: the callback ended after a partial upload.
  const contents = guest.streamNew();
  const abandoned = guest.request("/", { contents, method: { tag: "put" } });
  const sending = guest.send(abandoned.request, signal);
  await tick();
  const reader = requests[0]!.body!.getReader();
  const writing = guest.streamWrite(contents, encoder.encode("partial"), signal);
  expect(decoder.decode((await reader.read()).value)).toBe("partial");
  await writing;
  host.endCallback();
  const failure = { tag: "internal-error", val: "callback ended" } as const;
  const read = reader.read();
  await expect(read).rejects.toBeInstanceOf(HttpProviderError);
  await expect(read).rejects.toMatchObject({ code: failure });
  await expect(requests[0]!.trailers).rejects.toMatchObject({ code: failure });
  // The provider reports the failure it observed.
  answers[0]!(Promise.reject(new HttpProviderError(failure)));
  expect(await sending).toEqual({ tag: "err", val: failure });
  expect(await guest.futureRead(abandoned.result, RESULT)).toEqual({ tag: "err", val: failure });

  // A guest-reported trailers error.
  const failing = guest.futureNew(TRAILERS);
  const errored = guest.request("/", { trailers: failing, contents: guest.streamNew() });
  const pending = guest.send(errored.request, signal);
  await tick();
  const body = requests[1]!.body!.getReader();
  const reading = body.read();
  guest.futureWrite(failing, { tag: "err", val: { tag: "HTTP-request-trailer-section-size" } });
  host.endCallback();
  await expect(reading).rejects.toMatchObject({
    code: { tag: "HTTP-request-trailer-section-size" },
  });
  answers[1]!(
    Promise.reject(
      new HttpProviderError({ tag: "HTTP-request-trailer-section-size", val: undefined }),
    ),
  );
  expect((await pending).val).toEqual({ tag: "HTTP-request-trailer-section-size" });

  // A declared Content-Length is enforced while streaming: the excess chunk is never enqueued.
  const sized = guest.streamNew();
  const declared = guest.request("/", {
    headers: [["content-length", "3"]],
    contents: sized,
    trailers: guest.futureNew(TRAILERS),
    method: { tag: "post" },
  });
  const sizing = guest.send(declared.request, signal);
  await tick();
  const limited = requests[2]!.body!.getReader();
  const exceeding = limited.read();
  await guest.streamWrite(sized, encoder.encode("hello"), signal).catch(() => {});
  await expect(exceeding).rejects.toMatchObject({
    code: { tag: "HTTP-request-body-size", val: 5n },
  });
  answers[2]!(Promise.reject(new HttpProviderError({ tag: "HTTP-request-body-size", val: 5n })));
  expect((await sizing).val).toEqual({ tag: "HTTP-request-body-size", val: 5n });
  await retire(host);
});

test("uploads are bounded: guest writes wait for the provider's pulls", async () => {
  const { provider, requests, answers } = manual();
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;
  const contents = guest.streamNew();
  const { request } = guest.request("/", { contents, method: { tag: "post" } });
  const sending = guest.send(request, signal);
  await tick();
  let written = false;
  const writing = guest
    .streamWrite(contents, new Uint8Array(4 * 65_536).fill(7), signal)
    .then(() => {
      written = true;
    });
  await tick();
  await tick();
  // One buffered chunk plus one waiting chunk: the rest is not even copied out of its packet.
  expect(written).toBe(false);
  expect(host.inputBytes).toBe(2 * 65_536);
  const reader = requests[0]!.body!.getReader();
  let total = 0;
  while (total < 4 * 65_536) total += (await reader.read()).value!.byteLength;
  await writing;
  expect(host.inputBytes).toBe(0);
  answers[0]!({ status: 200, fields: [], body: null });
  expect((await sending).tag).toBe("ok");
  await retire(host);
});

test("the provider stopping the upload, invalid responses and typed codes are standard errors", async () => {
  const { provider, requests, answers } = manual();
  const { host, guest } = standard(provider);
  const signal = new AbortController().signal;

  // The provider cancels the request body: the guest's writes fail, the result reports it.
  const contents = guest.streamNew();
  const stopped = guest.request("/", { contents, method: { tag: "post" } });
  const sending = guest.send(stopped.request, signal);
  await tick();
  await requests[0]!.body!.cancel();
  expect(await guest.streamWrite(contents, encoder.encode("late"), signal)).toBe(WRITE_CLOSED);
  answers[0]!({ status: 413, fields: [], body: null });
  expect((await sending).tag).toBe("ok");
  expect(await guest.futureRead(stopped.result, RESULT)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "the HTTP provider cancelled the request body" },
  });

  // A malformed field or status is the provider's error; its body is still cancelled.
  for (const [response, message] of [
    [{ status: 200, fields: [["bad name", new Uint8Array()]] }, "returned an invalid field"],
    [{ status: 200, fields: [field("x-a", 0x0a)] }, "returned an invalid field"],
    [{ status: 42, fields: [] }, "returned an invalid status"],
  ] as const) {
    const body = source();
    const pending = guest.send(guest.request("/").request, signal);
    await tick();
    answers.at(-1)!({ ...response, body: body.body } as HttpProviderResponse);
    expect(await pending).toEqual({
      tag: "err",
      val: { tag: "internal-error", val: `the HTTP provider ${message}` },
    });
    await body.cancelled;
  }

  // A provider's typed code passes through; anything else is an internal error.
  const typed = guest.send(guest.request("/").request, signal);
  await tick();
  answers.at(-1)!(Promise.reject(new HttpProviderError({ tag: "TLS-certificate-error" })));
  expect(await typed).toEqual({ tag: "err", val: { tag: "TLS-certificate-error" } });
  const untyped = guest.send(guest.request("/").request, signal);
  await tick();
  answers.at(-1)!(Promise.reject(new Error("socket reset")));
  expect(await untyped).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "socket reset" },
  });
  await retire(host);
});

test("cancellation: before headers the signal aborts and a late response is cancelled to its ACK", async () => {
  const { provider, requests, answers } = manual();
  const { host, guest } = standard(provider);
  const callback = new AbortController();
  const contents = guest.streamNew();
  const { request, result } = guest.request("/", { contents, method: { tag: "post" } });
  const sending = guest.send(request, callback.signal);
  await tick();
  const seen = requests[0]!;
  const reader = seen.body!.getReader();
  const reading = reader.read();
  reading.catch(() => {});
  callback.abort();
  await expect(sending).rejects.toThrow("cancelled");
  expect(seen.signal.aborted).toBe(true);
  // The upload stops with an error, never a clean end.
  await expect(reading).rejects.toMatchObject({
    code: { tag: "internal-error", val: "request was cancelled" },
  });
  expect(await guest.futureRead(result, RESULT, new AbortController().signal)).toEqual({
    tag: "err",
    val: { tag: "internal-error", val: "request was cancelled" },
  });
  // The provider promise is physical work until it settles, then its body until the cancel ACK.
  let settled = false;
  const settling = host.settled().then(() => {
    settled = true;
  });
  await tick();
  expect(settled).toBe(false);
  const ack = deferred();
  const late = source(ack.promise);
  answers[0]!({ status: 200, fields: [], body: late.body });
  await late.cancelled;
  await tick();
  expect(settled).toBe(false);
  ack.resolve();
  await settling;
  await retire(host);
});

test("a retained raw response outlives callbacks; release aborts the signal and awaits the ACK", async () => {
  const { provider, requests, answers } = manual();
  const { host, guest } = standard(provider);
  const first = new AbortController();
  const sending = guest.send(guest.request("/sse").request, first.signal);
  await tick();
  const ack = deferred();
  const body = source(ack.promise);
  answers[0]!({ status: 200, fields: [], body: body.body });
  const response = (await sending).val;
  const [stream] = guest.sync("response", "consumeBody", response, guest.futureNew(RES));
  // A read in flight when the callback ends: the callback settles without the idle body.
  const pending = guest.streamRead(stream, first.signal);
  await tick();
  first.abort();
  await expect(pending).rejects.toThrow("cancelled");
  host.endCallback();
  await host.settled();
  expect(requests[0]!.signal.aborted).toBe(false);
  const second = new AbortController();
  body.controller.enqueue(encoder.encode("data: later\n\n"));
  expect(decoder.decode(await guest.streamRead(stream, second.signal))).toBe("data: later\n\n");
  guest.drop(stream);
  expect(requests[0]!.signal.aborted).toBe(true);
  await body.cancelled;
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
