// ADAPTER EVIDENCE ONLY: `MobileDriver` over a deterministic typed `MobileIngressCarrier`
// fixture that discriminates mapping, ordering, cancellation and late events. It is not
// on-device proof: no UniFFI, React Native bridge, Kotlin or Swift code runs here, and the
// React Native port does not provide the carrier yet. Real native execution of the same shared
// protocol is qualified on Node (`interpreter.node.test.mjs`).
import { expect, test } from "bun:test";
import {
  MobileDriver,
  type MobileIngressCarrier,
  type MobileIngressEvent,
  type MobilePort,
  type MobileRequestChunk,
} from "../src/interpreter/mobile.ts";
import {
  checkedRequest,
  HttpHandlerError,
  IngressExchange,
  type IncomingHttpRequest,
} from "../src/interpreter/http-ingress.ts";
import { refusedCode } from "../src/interpreter/native-ingress.ts";
import { encode, type HandleCodec, Writer } from "../src/interpreter/standard-codec.ts";
import { METHODS, TYPES } from "../src/interpreter/standard.ts";
import type { ExecutionScope } from "../src/interpreter/types.ts";

const bytes = (...values: number[]) => Uint8Array.from(values);
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const DISPATCH = { environment: 7, service: "handler", method: "handle" };

/** A scripted carrier: the test emits events; every call is journaled in order. */
function fixture(options: { ingress?: boolean } = {}) {
  const journal: unknown[] = [];
  const queue: MobileIngressEvent[] = [];
  const waiting: ((event: MobileIngressEvent) => void)[] = [];
  let failReceive: ((error: unknown) => void) | undefined;
  const carrier: MobileIngressCarrier = {
    beginIngress(packet, head) {
      journal.push(["begin", JSON.parse(packet), head]);
    },
    receiveIngress: () =>
      new Promise<MobileIngressEvent>((resolve, reject) => {
        const next = queue.shift();
        if (next) return resolve(next);
        waiting.push(resolve);
        failReceive = reject;
      }),
    supplyRequest(chunk: MobileRequestChunk) {
      journal.push(["supply", chunk]);
      return true;
    },
    pullResponse() {
      journal.push(["pull"]);
    },
    disconnect() {
      journal.push(["disconnect"]);
    },
  };
  const port: MobilePort = {
    description: () => "{}",
    begin() {
      throw new Error("ordinary begin is not used for ingress");
    },
    receive: async () => {
      throw new Error("ordinary receive is not used for ingress");
    },
    reply(id, packet) {
      journal.push(["reply", id, JSON.parse(packet)]);
      return true;
    },
    finish() {
      journal.push(["finish"]);
    },
    abort: async () => {
      journal.push(["abort"]);
    },
    release() {},
    cancel() {
      journal.push(["cancel"]);
    },
    close() {},
    ...(options.ingress === false ? {} : { ingress: carrier }),
  };
  const scope = (signal?: AbortSignal): ExecutionScope => ({
    ...(signal ? { signal } : {}),
    check() {},
    cancel() {
      journal.push(["scope.cancel"]);
    },
    sync: (request) => ({ sync: request.operation }),
    async: async (request) => {
      journal.push(["async", request.operation]);
      return { async: request.operation };
    },
  });
  return {
    journal,
    driver: new MobileDriver(port, () => {}),
    scope,
    emit(event: MobileIngressEvent) {
      const resolve = waiting.shift();
      if (resolve) resolve(event);
      else queue.push(event);
    },
    breakReceive(error: unknown) {
      failReceive?.(error);
    },
    kinds: () => journal.map((entry) => (entry as unknown[])[0]),
  };
}

/** A request body the test feeds; counts `next()` and `return()` calls. */
function body() {
  const chunks: Uint8Array[] = [];
  let ended = false;
  let wake: (() => void) | undefined;
  const state = { nexts: 0, returns: 0 };
  return {
    state,
    iterable: {
      [Symbol.asyncIterator]: () => ({
        async next(): Promise<IteratorResult<Uint8Array>> {
          state.nexts++;
          while (!chunks.length && !ended) await new Promise<void>((done) => (wake = done));
          const value = chunks.shift();
          return value ? { done: false, value } : { done: true, value: undefined };
        },
        async return(): Promise<IteratorResult<Uint8Array>> {
          state.returns++;
          ended = true;
          wake?.();
          return { done: true, value: undefined };
        },
      }),
    } as AsyncIterable<Uint8Array>,
    push(chunk: Uint8Array) {
      chunks.push(chunk);
      wake?.();
    },
    end() {
      ended = true;
      wake?.();
    },
  };
}

function exchange(extra: Partial<IncomingHttpRequest> = {}) {
  return new IngressExchange(
    checkedRequest({
      method: "POST",
      scheme: "https",
      authority: "app.test",
      pathWithQuery: "/sse?x=1",
      fields: [
        ["x-raw", bytes(0xff, 0x00)],
        ["x-raw", bytes(0x21)],
      ],
      body: null,
      ...extra,
    }),
  );
}

/** Run one exchange the way `Revision`/`Host.handleHttp` do: settle it once the call ended. */
function serve(f: ReturnType<typeof fixture>, served: IngressExchange, signal?: AbortSignal) {
  // As in `Host.handleHttp`, the head is the caller's promise; tests that never read it
  // still must not leave it unobserved.
  void served.head.catch(() => {});
  const call = f.driver.ingress!(DISPATCH, served, f.scope(signal));
  const ended = call.then(
    () => (served.settle(), "resolved"),
    (error) => (served.settle(error), error),
  );
  return { call, ended };
}

test("a port without the typed carrier exposes no ingress capability", () => {
  const f = fixture({ ingress: false });
  expect(f.driver.ingress).toBeUndefined();
  expect(fixture().driver.ingress).toBeFunction();
});

test("typed mapping: early head, request pulled on demand, raw bytes, trailers, finish last", async () => {
  const f = fixture();
  const request = body();
  const served = exchange({
    body: request.iterable,
    trailers: Promise.resolve([["x-req", bytes(0x80, 0xff)]]),
  });
  const { ended } = serve(f, served);
  await tick();
  expect(f.journal[0]).toEqual([
    "begin",
    { kind: "ingress", ...DISPATCH },
    {
      method: "POST",
      scheme: "https",
      authority: "app.test",
      pathWithQuery: "/sse?x=1",
      fields: [
        { name: "x-raw", value: bytes(0xff, 0x00) },
        { name: "x-raw", value: bytes(0x21) },
      ],
    },
  ]);
  // The head is published before the request body was read at all.
  f.emit({ kind: "head", status: 200, fields: [{ name: "x-out", value: bytes(0x80) }] });
  const response = await served.head;
  expect(request.state.nexts).toBe(0);
  expect(response.status).toBe(200);
  expect(response.fields).toEqual([["x-out", bytes(0x80)]]);
  // One pull: the application's next() is awaited; its bytes go out typed, unchanged.
  f.emit({ kind: "requestPull" });
  await tick();
  expect(request.state.nexts).toBe(1);
  expect(f.kinds()).not.toContain("supply");
  request.push(bytes(0, 1, 255));
  await tick();
  expect(f.journal.at(-1)).toEqual(["supply", { kind: "data", bytes: bytes(0, 1, 255) }]);
  expect((f.journal.at(-1) as [string, { bytes: unknown }])[1].bytes).toBeInstanceOf(Uint8Array);
  // Response frames are pulled only as the consumer reads.
  const reader = response.body[Symbol.asyncIterator]();
  expect(f.kinds().filter((kind) => kind === "pull")).toHaveLength(0);
  const first = reader.next();
  await tick();
  expect(f.kinds().filter((kind) => kind === "pull")).toHaveLength(1);
  f.emit({ kind: "data", bytes: bytes(9, 8, 7) });
  expect(await first).toEqual({ done: false, value: bytes(9, 8, 7) });
  // Request EOF answers the next pull with the application's trailers.
  f.emit({ kind: "requestPull" });
  request.end();
  await tick();
  expect(f.journal.at(-1)).toEqual([
    "supply",
    { kind: "end", trailers: [{ name: "x-req", value: bytes(0x80, 0xff) }] },
  ]);
  // A host request on the same call authority; the async one is drained before finish.
  f.emit({ kind: "host", id: 1, asynchronous: false, packet: '{"operation":"orb"}' });
  f.emit({
    kind: "host",
    id: 2,
    asynchronous: true,
    packet: '{"operation":"wait","milliseconds":0}',
  });
  // Trailers then end: two pulls inside one consumer read.
  const last = reader.next();
  await tick();
  f.emit({ kind: "trailers", fields: [{ name: "x-done", value: bytes(0x31) }] });
  await tick();
  f.emit({ kind: "end" });
  expect(await last).toEqual({ done: true, value: undefined });
  expect(await response.trailers).toEqual([["x-done", bytes(0x31)]]);
  f.emit({ kind: "complete", result: '{"ok":"null"}' });
  expect(await ended).toBe("resolved");
  const order = f.kinds();
  expect(order.indexOf("reply")).toBeLessThan(order.indexOf("scope.cancel"));
  expect(order.lastIndexOf("reply")).toBeLessThan(order.indexOf("finish"));
  expect(order.indexOf("scope.cancel")).toBeLessThan(order.indexOf("finish"));
  expect(order.at(-1)).toBe("finish");
  expect(f.journal.filter((entry) => (entry as unknown[])[0] === "reply")).toEqual([
    ["reply", 1, { ok: { sync: "orb" } }],
    ["reply", 2, { ok: { async: "wait" } }],
  ]);
  // The fully read request is not returned.
  expect(request.state.returns).toBe(0);
});

test("consumer return() disconnects once; late frames and pulls after complete are inert", async () => {
  const f = fixture();
  const request = body();
  const served = exchange({ body: request.iterable });
  const { ended } = serve(f, served);
  f.emit({ kind: "head", status: 200, fields: [] });
  const response = await served.head;
  f.emit({ kind: "requestPull" });
  await tick();
  const reader = response.body[Symbol.asyncIterator]();
  await reader.return!();
  await reader.return!();
  expect(f.kinds().filter((kind) => kind === "disconnect")).toHaveLength(1);
  expect(request.state.returns).toBe(1);
  expect(await response.trailers.catch((error) => error.code)).toEqual({
    tag: "connection-terminated",
  });
  await expect(reader.next()).rejects.toBeInstanceOf(HttpHandlerError);
  // Late carrier events for the dropped body are ignored.
  f.emit({ kind: "error", message: "late" });
  f.emit({ kind: "complete", result: '{"ok":"null"}' });
  expect(await ended).toBe("resolved");
  // The application's pending next() resolves after completion: nothing is supplied.
  request.push(bytes(1));
  await tick();
  expect(f.kinds().filter((kind) => kind === "supply")).toHaveLength(0);
  expect(f.kinds().at(-1)).toBe("finish");
});

test("signal cancellation reaches the native call; the body and trailers fail, then finish", async () => {
  const f = fixture();
  const controller = new AbortController();
  const served = exchange();
  const { ended } = serve(f, served, controller.signal);
  f.emit({ kind: "head", status: 200, fields: [] });
  const response = await served.head;
  const reader = response.body[Symbol.asyncIterator]();
  const pending = reader.next();
  await tick();
  controller.abort();
  expect(f.kinds()).toContain("cancel");
  // Host-driven end: complete arrives while the pull is unanswered.
  f.emit({
    kind: "complete",
    result: JSON.stringify({
      error: { code: "cancelled", reason: "root", message: "callback cancelled" },
    }),
  });
  const failure = await pending.catch((error) => error);
  expect(String(failure)).toContain("callback cancelled");
  await expect(response.trailers).rejects.toBe(failure);
  expect(String(await ended)).toContain("callback cancelled");
  expect(f.kinds().at(-1)).toBe("finish");
  // A cancellation after completion never reaches the next exchange.
  controller.abort();
  expect(f.kinds().filter((kind) => kind === "cancel")).toHaveLength(1);
});

test("an ok completion without a terminal frame fails the body instead of truncating it", async () => {
  const f = fixture();
  const served = exchange();
  const { ended } = serve(f, served);
  f.emit({ kind: "head", status: 200, fields: [] });
  const response = await served.head;
  const reader = response.body[Symbol.asyncIterator]();
  const pending = reader.next();
  await tick();
  f.emit({ kind: "complete", result: '{"ok":"null"}' });
  await expect(pending).rejects.toThrow("the HTTP handler callback ended");
  await expect(response.trailers).rejects.toThrow("the HTTP handler callback ended");
  expect(await ended).toBe("resolved");
});

/** Standard-codec bytes of one `error-code`, encoded by the generated table, as native sends. */
const ERROR_CODE = (() => {
  const send = METHODS.find((m) => m.interface === "wasi:http/client@0.3.0" && m.name === "send")!;
  return TYPES[send.result!]![2] as number;
})();
const NO_HANDLES: HandleCodec = {
  handle: () => {
    throw new Error("no handles");
  },
  value: () => {
    throw new Error("no handles");
  },
};
function codeBytes(code: unknown): Uint8Array {
  const writer = new Writer();
  encode(writer, ERROR_CODE, code, NO_HANDLES);
  return writer.finish();
}
const failed = (message: string) =>
  JSON.stringify({ error: { code: "unavailable", reason: "wasm_executor", message } });

test("errors before the head are reported only after settlement, typed only from bytes", async () => {
  const denied = codeBytes({ tag: "HTTP-request-denied" });
  for (const [events, head, call] of [
    // A refusal: the head reports the typed code; the call settles with its own completion.
    [
      [
        { kind: "refused", code: denied },
        { kind: "complete", result: failed("untyped repeat") },
      ],
      (error: unknown) =>
        expect((error as HttpHandlerError).code).toEqual({ tag: "HTTP-request-denied" }),
      (error: unknown) => expect(String(error)).toContain("untyped repeat"),
    ],
    // A trap after the refusal stays the call's failure.
    [
      [
        { kind: "refused", code: denied },
        { kind: "complete", result: failed("component trap") },
      ],
      (error: unknown) => expect(error).toBeInstanceOf(HttpHandlerError),
      (error: unknown) => {
        expect(error).not.toBeInstanceOf(HttpHandlerError);
        expect(String(error)).toContain("component trap");
      },
    ],
    // Completion text is never parsed: without typed bytes there is no handler code.
    [
      [
        {
          kind: "complete",
          result: failed("the incoming HTTP handler returned ErrorCode::HttpRequestDenied"),
        },
      ],
      (error: unknown) => expect(error).not.toBeInstanceOf(HttpHandlerError),
      (error: unknown) => expect(error).not.toBeInstanceOf(HttpHandlerError),
    ],
    [
      [{ kind: "complete", result: '{"ok":"null"}' }],
      (error: unknown) => expect(String(error)).toContain("ended without a response"),
      (outcome: unknown) => expect(outcome).toBe("resolved"),
    ],
  ] as const) {
    const f = fixture();
    const request = body();
    const served = exchange({ body: request.iterable });
    let observed = false;
    void served.head.catch(() => (observed = true));
    const { ended } = serve(f, served);
    await tick();
    for (const event of events) {
      f.emit(event as MobileIngressEvent);
      await tick();
      // Nothing is reported before the physical call settled.
      if (event.kind !== "complete") expect(observed).toBe(false);
    }
    call(await ended);
    head(await served.head.catch((error) => error));
    expect(f.kinds().at(-1)).toBe("finish");
    // The unread request body was returned once.
    expect(request.state.returns).toBe(1);
  }
});

test("typed refusals keep every payload shape exactly", async () => {
  for (const code of [
    { tag: "DNS-timeout" },
    { tag: "DNS-error", val: { rcode: 'q"\n é🌍', infoCode: 65535 } },
    { tag: "TLS-alert-received", val: { alertId: 255, alertMessage: "" } },
    { tag: "HTTP-request-body-size", val: 18446744073709551615n },
    { tag: "HTTP-response-body-size", val: 9007199254740993n },
    { tag: "HTTP-request-body-size" },
    { tag: "HTTP-request-header-size", val: { fieldName: "x-\u0000", fieldSize: 4294967295 } },
    { tag: "HTTP-response-trailer-size", val: {} },
    { tag: "HTTP-response-content-coding", val: "br" },
    { tag: "internal-error", val: 'quote " backslash \\ newline \n unicode é🌍 nul \u0000' },
    { tag: "internal-error" },
    { tag: "configuration-error" },
  ]) {
    expect(refusedCode(codeBytes(code))).toEqual(code as never);
    const f = fixture();
    const served = exchange();
    const { ended } = serve(f, served);
    f.emit({ kind: "refused", code: codeBytes(code) });
    f.emit({ kind: "complete", result: failed("refused") });
    await ended;
    const error = await served.head.catch((failure) => failure);
    expect(error).toBeInstanceOf(HttpHandlerError);
    expect((error as HttpHandlerError).code).toEqual(code as never);
  }
});

test("malformed refused bytes are a transport failure, never a guessed code", async () => {
  const valid = codeBytes({ tag: "HTTP-request-denied" });
  for (const code of [
    valid.subarray(0, 2),
    Uint8Array.of(...valid, 0),
    Uint8Array.of(255, 0, 0, 0),
    [15, 0, 0, 0] as unknown as Uint8Array,
  ]) {
    const f = fixture();
    const served = exchange();
    const { ended } = serve(f, served);
    f.emit({ kind: "refused", code });
    f.emit({ kind: "complete", result: failed("refused") });
    const failure = await ended;
    const head = await served.head.catch((error) => error);
    expect(head).not.toBeInstanceOf(HttpHandlerError);
    expect(head).toBe(failure);
    expect(f.kinds()).toContain("disconnect");
    expect(f.kinds().at(-1)).toBe("finish");
  }
  // A refusal after a published head is a protocol violation, not a second answer.
  const f = fixture();
  const served = exchange();
  const { ended } = serve(f, served);
  f.emit({ kind: "head", status: 200, fields: [] });
  const response = await served.head;
  f.emit({ kind: "refused", code: valid });
  f.emit({ kind: "complete", result: '{"ok":"null"}' });
  expect(String(await ended)).toContain("cannot have a head");
  await expect(response.trailers).rejects.toThrow("cannot have a head");
});

test("an application body that never settles cannot hold the call", async () => {
  const f = fixture();
  const orphan = { nexts: 0, returns: 0 };
  const served = exchange({
    body: {
      [Symbol.asyncIterator]: () => ({
        next: () => (orphan.nexts++, new Promise<IteratorResult<Uint8Array>>(() => {})),
        return: () => (orphan.returns++, new Promise<IteratorResult<Uint8Array>>(() => {})),
      }),
    },
  });
  const { ended } = serve(f, served);
  f.emit({ kind: "requestPull" });
  await tick();
  f.emit({ kind: "complete", result: '{"ok":"null"}' });
  await ended;
  expect(orphan).toEqual({ nexts: 1, returns: 1 });
  expect(f.kinds().at(-1)).toBe("finish");
});

test("bytes that are not Uint8Array are refused, never converted; the call still finishes", async () => {
  const f = fixture();
  const served = exchange();
  const { ended } = serve(f, served);
  f.emit({ kind: "head", status: 200, fields: [] });
  const response = await served.head;
  const reader = response.body[Symbol.asyncIterator]();
  const pending = reader.next();
  await tick();
  f.emit({ kind: "data", bytes: [1, 2, 3] as unknown as Uint8Array });
  await tick();
  // The malformed frame fails the call: the guest is disconnected and cancelled.
  expect(f.kinds()).toContain("disconnect");
  expect(f.kinds()).toContain("cancel");
  f.emit({ kind: "end" });
  f.emit({ kind: "complete", result: '{"ok":"null"}' });
  const failure = await ended;
  expect(String(failure)).toContain("Uint8Array");
  await expect(pending).rejects.toThrow("Uint8Array");
  await expect(response.trailers).rejects.toThrow("Uint8Array");
  expect(f.kinds().at(-1)).toBe("finish");
});

test("a broken receive loop ends both bodies, drains host work and quarantines", async () => {
  const f = fixture();
  const request = body();
  const served = exchange({ body: request.iterable });
  const { ended } = serve(f, served);
  f.emit({ kind: "head", status: 200, fields: [] });
  const response = await served.head;
  f.emit({ kind: "requestPull" });
  await tick();
  f.breakReceive(new Error("bridge lost"));
  expect(String(await ended)).toContain("bridge lost");
  await expect(response.trailers).rejects.toThrow("bridge lost");
  expect(request.state.returns).toBe(1);
  expect(f.kinds()).toContain("abort");
  expect(f.kinds()).not.toContain("finish");
});

test("a head refused by the native carrier fails the call and returns the body", async () => {
  const f = fixture();
  const request = body();
  const served = exchange({ body: request.iterable });
  (f.driver.port.ingress as MobileIngressCarrier).beginIngress = () => {
    throw new Error("invalid request method");
  };
  const { ended } = serve(f, served);
  expect(String(await ended)).toContain("invalid request method");
  expect(request.state.returns).toBe(1);
  expect(f.kinds()).not.toContain("finish");
});

/** A body whose iterator factory throws, as a locked WHATWG stream's reader does (M-1). */
function lockedBody() {
  const state = { factories: 0 };
  const iterable = {
    [Symbol.asyncIterator](): AsyncIterator<Uint8Array> {
      state.factories++;
      throw new TypeError("ReadableStream is locked");
    },
  } as AsyncIterable<Uint8Array>;
  return { state, iterable };
}

test("a body whose iterator factory throws never strands the call (M-1)", async () => {
  // Unread completion: the handler answers without reading; the call still finishes.
  {
    const f = fixture();
    const locked = lockedBody();
    const served = exchange({ body: locked.iterable });
    const { ended } = serve(f, served);
    f.emit({ kind: "head", status: 204, fields: [] });
    await served.head;
    f.emit({ kind: "complete", result: '{"ok":"null"}' });
    expect(await ended).toBe("resolved");
    expect(f.kinds().at(-1)).toBe("finish");
    expect(locked.state.factories).toBe(1);
  }
  // A head refused before admission: the refusal is reported, not the application's throw.
  {
    const f = fixture();
    const locked = lockedBody();
    const served = exchange({ body: locked.iterable });
    (f.driver.port.ingress as MobileIngressCarrier).beginIngress = () => {
      throw new Error("invalid request method");
    };
    const { ended } = serve(f, served);
    expect(String(await ended)).toContain("invalid request method");
    expect(locked.state.factories).toBe(1);
  }
  // A consumer disconnect still reaches the guest; return() itself does not reject.
  {
    const f = fixture();
    const locked = lockedBody();
    const served = exchange({ body: locked.iterable });
    const { ended } = serve(f, served);
    f.emit({ kind: "head", status: 200, fields: [] });
    const response = await served.head;
    expect(await response.body[Symbol.asyncIterator]().return!()).toEqual({
      done: true,
      value: undefined,
    });
    expect(f.kinds()).toContain("disconnect");
    f.emit({ kind: "complete", result: '{"ok":"null"}' });
    expect(await ended).toBe("resolved");
    expect(f.kinds().at(-1)).toBe("finish");
    expect(locked.state.factories).toBe(1);
  }
  // A broken receive loop still quarantines the native exchange.
  {
    const f = fixture();
    const locked = lockedBody();
    const served = exchange({ body: locked.iterable });
    const { ended } = serve(f, served);
    f.emit({ kind: "head", status: 200, fields: [] });
    await served.head;
    await tick();
    f.breakReceive(new Error("bridge lost"));
    expect(String(await ended)).toContain("bridge lost");
    expect(f.kinds()).toContain("abort");
    expect(locked.state.factories).toBe(1);
  }
  // A throwing return() is contained the same way, and called once.
  {
    const f = fixture();
    let returns = 0;
    const served = exchange({
      body: {
        [Symbol.asyncIterator]: () => ({
          next: async () => ({ done: true, value: undefined }),
          return() {
            returns++;
            throw new Error("cleanup failed");
          },
        }),
      },
    });
    const { ended } = serve(f, served);
    f.emit({ kind: "complete", result: '{"ok":"null"}' });
    await ended;
    expect(returns).toBe(1);
    expect(f.kinds().at(-1)).toBe("finish");
  }
});
