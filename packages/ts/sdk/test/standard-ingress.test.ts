// Incoming `wasi:http` in the browser StandardHost, driven over the exact guest wire: the request
// handle issued to a handler callback, INGRESS_HEAD publishing the response head early and
// completing only when its delivery ended, and every end of that delivery.
import { expect, test } from "bun:test";
import {
  checkedRequest,
  HttpHandlerError,
  type HttpHandlerResponse,
  type IncomingHttpRequest,
  IngressExchange,
} from "../src/interpreter/http-ingress.ts";
import { METHODS } from "../src/interpreter/standard.ts";
import { encode, INGRESS_HEAD, Writer } from "../src/interpreter/standard-codec.ts";
import type { StandardHost } from "../src/interpreter/standard-host.ts";
import {
  BODY,
  decoder,
  encoder,
  type Guest,
  method,
  OUT_TRAILERS,
  raw,
  RES,
  RESULT,
  retire,
  standard,
  tick,
  TRAILERS,
} from "./standard-guest.ts";

const HANDLER_RESULT = METHODS[method(undefined, "send")]!.result!;

function head(result: unknown): Uint8Array {
  const writer = new Writer();
  writer.u8(0);
  writer.u8(INGRESS_HEAD);
  encode(writer, HANDLER_RESULT, result, raw);
  return writer.finish();
}

/** An application request body it can feed and observe. */
function source(chunks: Uint8Array[] = []) {
  const queue = [...chunks];
  let ended = false;
  let wake: (() => void) | undefined;
  const state = { returned: 0, pulls: 0 };
  const body: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]() {
      return {
        async next() {
          state.pulls++;
          while (!queue.length && !ended)
            await new Promise<void>((resolve) => {
              wake = resolve;
            });
          const value = queue.shift();
          return value ? { done: false, value } : { done: true, value: undefined };
        },
        async return() {
          state.returned++;
          ended = true;
          wake?.();
          return { done: true, value: undefined };
        },
      };
    },
  };
  return {
    body,
    state,
    push(chunk: Uint8Array) {
      queue.push(chunk);
      wake?.();
    },
    end() {
      ended = true;
      wake?.();
    },
  };
}

function incoming(overrides: Partial<IncomingHttpRequest> = {}): IncomingHttpRequest {
  return {
    method: "POST",
    scheme: "https",
    authority: "app.example.test",
    pathWithQuery: "/events?x=1",
    fields: [
      ["x-raw", Uint8Array.of(255, 0x20, 1 + 0x40)],
      ["x-raw", Uint8Array.of(254)],
    ],
    body: null,
    ...overrides,
  };
}

/** The guest builds a streaming response and publishes it. */
function respond(guest: Guest, fields: [string, string | number[]][] = []) {
  const contents = guest.streamNew();
  const trailers = guest.futureNew(TRAILERS);
  const [response, transmitted] = guest.sync(
    "response",
    "new",
    guest.fields(fields),
    contents,
    trailers,
  );
  return { response, transmitted, contents, trailers };
}

async function collect(body: AsyncIterable<Uint8Array>): Promise<string> {
  let text = "";
  for await (const chunk of body) text += decoder.decode(chunk);
  return text;
}

function serving(request = incoming()) {
  const { host, guest } = standard();
  const exchange = new IngressExchange(checkedRequest(request));
  const handle = host.beginIngress(exchange);
  return { host, guest, exchange, handle };
}

async function finish(host: StandardHost, exchange: IngressExchange, failure?: unknown) {
  host.endIngress(failure ?? new Error("callback ended"));
  // A request answered without a head reports why; these tests observe it where it matters.
  exchange.head.catch(() => {});
  exchange.settle(failure);
  await retire(host);
}

test("the head is published at once; the operation completes only after the delivery ended", async () => {
  const { host, guest, exchange, handle } = serving();
  // The handler sees the request target and raw duplicate fields exactly.
  expect(guest.sync("request", "getMethod", handle)).toEqual({ tag: "post" });
  expect(guest.sync("request", "getPathWithQuery", handle)).toBe("/events?x=1");
  expect(guest.sync("request", "getAuthority", handle)).toBe("app.example.test");
  const headers = guest.sync("request", "getHeaders", handle);
  expect(guest.sync("fields", "get", headers, "x-raw")).toEqual([
    Uint8Array.of(255, 0x20, 0x41),
    Uint8Array.of(254),
  ]);
  const { response, transmitted, contents, trailers } = respond(guest, [
    ["content-type", "text/event-stream"],
  ]);
  let completed = false;
  const published = host.async(head({ tag: "ok", val: response })).then((delivery) => {
    completed = true;
    return delivery;
  });
  const reply = await exchange.head;
  expect(reply.status).toBe(200);
  expect(reply.fields.map(([name, value]) => [name, decoder.decode(value)])).toEqual([
    ["content-type", "text/event-stream"],
  ]);
  // The transferred response is the host's now.
  expect(() => guest.drop(response)).toThrow("unknown or dropped handle");
  const iterator = reply.body[Symbol.asyncIterator]();
  const first = iterator.next();
  await guest.streamWrite(contents, encoder.encode("data: 1\n\n"));
  expect(decoder.decode((await first).value)).toBe("data: 1\n\n");
  await tick();
  expect(completed).toBe(false);
  // The transmission result is not settled before the delivery ended.
  let transmittedResult: unknown;
  const reading = guest.futureRead(transmitted, RESULT).then((value) => {
    transmittedResult = value;
  });
  await guest.streamWrite(contents, encoder.encode("data: 2\n\n"));
  expect(decoder.decode((await iterator.next()).value)).toBe("data: 2\n\n");
  guest.drop(contents);
  const end = iterator.next();
  await tick();
  expect(transmittedResult).toBeUndefined();
  guest.futureWrite(trailers, {
    tag: "ok",
    val: guest.fields([["x-finished", [0x21, 255]]]),
  });
  expect((await end).done).toBe(true);
  expect(await reply.trailers).toEqual([["x-finished", Uint8Array.of(0x21, 255)]]);
  expect((await published).bytes.length).toBe(0);
  await reading;
  expect(transmittedResult).toEqual({ tag: "ok" });
  guest.drop(handle);
  await finish(host, exchange);
});

test("the request body streams as the handler reads, with its trailers", async () => {
  const body = source();
  const { host, guest, exchange, handle } = serving(
    incoming({ body: body.body, trailers: Promise.resolve([["x-sum", Uint8Array.of(0x37)]]) }),
  );
  const res = guest.futureNew(RES);
  const [stream, trailers] = guest.sync("request", "consumeBody", handle, res);
  expect(body.state.pulls).toBe(0);
  const read = guest.streamRead(stream);
  await tick();
  body.push(Uint8Array.of(1, 2, 3));
  expect(await read).toEqual(Uint8Array.of(1, 2, 3));
  body.push(Uint8Array.of(4));
  body.end();
  expect(await guest.streamRead(stream)).toEqual(Uint8Array.of(4));
  expect(await guest.streamRead(stream)).toBeUndefined();
  const received = await guest.futureRead(trailers, OUT_TRAILERS);
  expect(received.tag).toBe("ok");
  expect(guest.sync("fields", "get", received.val, "x-sum")).toEqual([Uint8Array.of(0x37)]);
  expect(BODY.length).toBe(2);
  for (const owned of [received.val, stream]) guest.drop(owned);
  guest.futureWrite(res, { tag: "ok" });
  await finish(host, exchange);
});

test("a disconnect while the handler writes: it sees connection-terminated and its writes fail", async () => {
  const body = source();
  const { host, guest, exchange, handle } = serving(incoming({ body: body.body }));
  const res = guest.futureNew(RES);
  const [requestStream] = guest.sync("request", "consumeBody", handle, res);
  const { response, transmitted, contents } = respond(guest);
  const published = host.async(head({ tag: "ok", val: response }));
  const reply = await exchange.head;
  const iterator = reply.body[Symbol.asyncIterator]();
  const first = iterator.next();
  await guest.streamWrite(contents, encoder.encode("data: 1\n\n"));
  await first;
  // The client goes away.
  await iterator.return!();
  await published;
  expect(await guest.futureRead(transmitted, RESULT)).toEqual({
    tag: "err",
    val: { tag: "connection-terminated" },
  });
  // The next write fails; the request body ends, its trailers carrying the same code.
  await expect(guest.streamWrite(contents, encoder.encode("data: 2\n\n"))).rejects.toThrow();
  expect(await guest.streamRead(requestStream)).toBeUndefined();
  await host.settled();
  expect(body.state.returned).toBe(1);
  await expect(reply.trailers).rejects.toBeInstanceOf(Error);
  for (const owned of [contents, requestStream]) guest.drop(owned);
  guest.futureWrite(res, { tag: "ok" });
  await finish(host, exchange);
});

test("a disconnect while the handler idles in a request read ends that read", async () => {
  const body = source();
  const { host, guest, exchange, handle } = serving(incoming({ body: body.body }));
  const res = guest.futureNew(RES);
  const [requestStream, requestTrailers] = guest.sync("request", "consumeBody", handle, res);
  const { response, transmitted } = respond(guest);
  const published = host.async(head({ tag: "ok", val: response }));
  const reply = await exchange.head;
  const idle = guest.streamRead(requestStream);
  await tick();
  expect(body.state.pulls).toBe(1);
  await reply.body[Symbol.asyncIterator]().return!();
  // The pending read ends the stream; the trailers future says why.
  expect(await idle).toBeUndefined();
  expect(await guest.futureRead(requestTrailers, OUT_TRAILERS)).toEqual({
    tag: "err",
    val: { tag: "connection-terminated" },
  });
  expect(await guest.futureRead(transmitted, RESULT)).toEqual({
    tag: "err",
    val: { tag: "connection-terminated" },
  });
  await published;
  await host.settled();
  expect(body.state.returned).toBe(1);
  guest.drop(requestStream);
  guest.futureWrite(res, { tag: "ok" });
  await finish(host, exchange);
});

test("an error code is reported only when the call settles; the operation completes at once", async () => {
  const { host, guest, exchange, handle } = serving();
  let rejected: unknown;
  exchange.head.catch((error) => {
    rejected = error;
  });
  await host.async(head({ tag: "err", val: { tag: "HTTP-request-denied" } }));
  await tick();
  expect(rejected).toBeUndefined();
  guest.drop(handle);
  await finish(host, exchange);
  expect(rejected).toBeInstanceOf(HttpHandlerError);
  expect((rejected as HttpHandlerError).code).toEqual({ tag: "HTTP-request-denied" });
});

test("cancellation ends the delivery: reads reject with the reason and the request closes", async () => {
  const body = source();
  const { host, guest, exchange, handle } = serving(incoming({ body: body.body }));
  const { response, contents } = respond(guest);
  const cancel = new AbortController();
  const published = host.async(head({ tag: "ok", val: response }), cancel.signal);
  const reply = await exchange.head;
  const iterator = reply.body[Symbol.asyncIterator]();
  const pending = iterator.next();
  const read = pending.then(
    () => "delivered",
    (error: Error) => error.message,
  );
  cancel.abort();
  await expect(published).rejects.toThrow("Operation cancelled");
  expect(await read).toBe("Operation cancelled");
  await expect(reply.trailers).rejects.toThrow("Operation cancelled");
  await expect(iterator.next()).rejects.toThrow();
  await expect(guest.streamWrite(contents, encoder.encode("late"))).rejects.toThrow();
  guest.drop(contents);
  guest.drop(handle);
  await finish(host, exchange, new Error("cancelled"));
  expect(body.state.returned).toBe(1);
});

test("refusals: one head per request, none outside a request, branded exchanges, valid input", async () => {
  const { host, guest } = standard();
  const { response } = respond(guest);
  // INGRESS_HEAD outside an incoming request is refused and transfers nothing.
  await expect(host.async(head({ tag: "ok", val: response }))).rejects.toThrow(
    "no incoming HTTP request is being served",
  );
  const exchange = new IngressExchange(checkedRequest(incoming()));
  const request = host.beginIngress(exchange);
  expect(() => host.beginIngress(new IngressExchange(checkedRequest(incoming())))).toThrow(
    "already being served",
  );
  await host.async(head({ tag: "err", val: { tag: "internal-error" } }));
  await expect(host.async(head({ tag: "ok", val: response }))).rejects.toThrow(
    "the response head was already published",
  );
  // Only exchanges issued by the host enter the handler, each exactly once.
  expect(() => IngressExchange.claim({ request: incoming() })).toThrow(
    "is served only through Host.handleHttp",
  );
  expect(IngressExchange.claim(exchange)).toBe(exchange);
  expect(() => IngressExchange.claim(exchange)).toThrow("already handled");
  guest.drop(request);
  host.endIngress(new Error("done"));
  exchange.settle();
  await expect(exchange.head).rejects.toBeInstanceOf(HttpHandlerError);
  // Invalid input is refused before a handle is issued.
  const before = host.size;
  for (const bad of [
    incoming({ method: "BAD METHOD" }),
    incoming({ pathWithQuery: "no-slash" }),
    incoming({ authority: "a b" }),
    incoming({ fields: [["bad name", Uint8Array.of(1)]] }),
    incoming({ fields: [["x", Uint8Array.of(10)]] }),
  ])
    expect(() => host.beginIngress(new IngressExchange(checkedRequest(bad)))).toThrow(TypeError);
  expect(host.size).toBe(before);
  expect(() => checkedRequest({ ...incoming(), body: "text" as never })).toThrow(TypeError);
  guest.drop(response);
  await retire(host);
});

test("retirement aborts an unfinished delivery and closes the request body", async () => {
  const body = source();
  const { host, guest, exchange, handle } = serving(incoming({ body: body.body }));
  const { response } = respond(guest);
  const published = host.async(head({ tag: "ok", val: response }));
  const reply: HttpHandlerResponse = await exchange.head;
  published.catch(() => {});
  await host.dispose();
  await expect(collect(reply.body)).rejects.toThrow("Standard host is closed");
  await host.settled();
  expect(body.state.returned).toBe(1);
  expect(host.size).toBe(0);
  expect(host.pending).toBe(0);
  expect(host.inputBytes).toBe(0);
  void handle;
});

test("a handler-only revision has no outgoing HTTP: client.send is refused before any provider", async () => {
  let sent = 0;
  const { StandardHost } = await import("../src/interpreter/standard-host.ts");
  const { Guest } = await import("./standard-guest.ts");
  // A handler-only Orb imports `wasi:http/types` (bound to the reserved ingress provider) only.
  const host = new StandardHost(new Set(["wasi:http/types@0.3.0"]), [], 8 << 20, {
    http: {
      provider: () => ({
        send: async () => {
          sent++;
          throw new Error("unreachable");
        },
      }),
    },
  });
  const guest = new Guest(host);
  const exchange = new IngressExchange(checkedRequest(incoming()));
  const handle = host.beginIngress(exchange);
  await expect(host.async(guest.packet(undefined, "send", [handle]))).rejects.toThrow(
    "standard interface is not imported by this Orb",
  );
  expect(sent).toBe(0);
  guest.drop(handle);
  await finish(host, exchange);
});

test("a body whose iterator factory throws is closed as tracked cleanup, never escaping (M-1)", async () => {
  // A locked WHATWG stream throws from getReader() in its iterator factory.
  const state = { factories: 0, returns: 0 };
  const locked: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]() {
      state.factories++;
      throw new TypeError("ReadableStream is locked");
    },
  };
  // Never read: the callback ends with an error code, then the host retires cleanly.
  {
    const { host, guest, exchange, handle } = serving(incoming({ body: locked }));
    await host.async(head({ tag: "err", val: { tag: "HTTP-request-denied" } }));
    guest.drop(handle);
    await finish(host, exchange);
    expect(state.factories).toBe(1);
  }
  // A throwing return() is the same: called once, tracked, settled.
  const throwing: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]: () => ({
      next: () => new Promise<IteratorResult<Uint8Array>>(() => {}),
      return() {
        state.returns++;
        throw new Error("cleanup failed");
      },
    }),
  };
  {
    const { host, exchange } = serving(incoming({ body: throwing }));
    await finish(host, exchange, new Error("cancelled"));
    expect(state.returns).toBe(1);
  }
});

test("a request the browser host refuses at hand-off closes the body it took, once (L-1)", async () => {
  for (const overrides of [
    { fields: [["bad name", Uint8Array.of(0x41)] as const] },
    { method: "BAD METHOD" },
  ]) {
    const body = source();
    const { host } = standard();
    const exchange = new IngressExchange(
      checkedRequest(incoming({ ...overrides, body: body.body })),
    );
    expect(() => host.beginIngress(exchange)).toThrow(/invalid incoming/);
    exchange.head.catch(() => {});
    // The host owned the body; settlement does not open or close it a second time.
    exchange.settle(new Error("refused"));
    await retire(host);
    expect(body.state.returned).toBe(1);
  }
});
