// The real guest bootstrap (effect bridge and generated standard bindings) serving incoming HTTP
// against the real browser StandardHost. A small effect loop stands in for `BrowserPort.execute`,
// and `ledger.note` stands in for a context-checked dependency call: the host refuses it once
// the callback ended. What this proves: the head leaves early, the callback lives until the
// delivery ended and its own post-head work finished, and every disconnect ends it.
import { expect, test } from "bun:test";
import path from "node:path";
import { createContext, runInContext } from "node:vm";
import {
  checkedRequest,
  HttpHandlerError,
  type IncomingHttpRequest,
  IngressExchange,
} from "../src/interpreter/http-ingress.ts";
import { StandardHost } from "../src/interpreter/standard-host.ts";

const TYPES = "wasi:http/types@0.3.0";
const READ_CHUNK = Symbol.for("tembra.wasi.stream.read-chunk");
const WRITE_CHUNKS = Symbol.for("tembra.wasi.stream.write-chunks");
const scripts = await Promise.all(
  ["bridge.ts", "standard.ts"].map(async (file) => {
    const built = await Bun.build({
      entrypoints: [
        path.resolve(import.meta.dir, "../../../../crates/executor-wasm/guest/src", file),
      ],
      format: "iife",
      target: "node",
    });
    if (!built.success) throw new AggregateError(built.logs);
    return built.outputs[0]!.text();
  }),
);
const text = (value: string) => Uint8Array.from(value, (unit) => unit.charCodeAt(0));
const decode = (bytes: Uint8Array) => String.fromCharCode(...bytes);
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
const ok = Promise.resolve({ tag: "ok" as const, val: undefined });

/** A request body the test feeds; `returned` counts the host's `return()` calls. */
function source() {
  const queue: Uint8Array[] = [];
  let ended = false;
  let wake: (() => void) | undefined;
  const state = { returned: 0 };
  const body: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]: () => ({
      async next() {
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
    }),
  };
  return {
    body,
    state,
    push(chunk: string) {
      queue.push(text(chunk));
      wake?.();
    },
    end() {
      ended = true;
      wake?.();
    },
  };
}

function engine() {
  const standard = new StandardHost(new Set([TYPES]), [], 8 << 20);
  let alive = false;
  const ledger: string[] = [];
  const globals: any = {
    Uint8Array,
    DataView,
    Symbol,
    __scopeActive: () => alive,
    __tembraContract: { imports: [{ contract: TYPES, type_only: false }] },
    __hostSyncBytes: (packet: Uint8Array) => standard.sync(packet),
  };
  const context = createContext(globals);
  for (const script of scripts) runInContext(script, context);
  // Guest-side helpers with the runtime's shapes: JSON host operations through the bridge.
  globals.tembra = {
    wait: (ms: number) =>
      globals.__hostAsync(JSON.stringify({ operation: "wait", milliseconds: ms })),
  };
  const note = (entry: string): Promise<number> =>
    globals.__hostAsync(JSON.stringify({ operation: "ledger.note", entry })).then(Number);
  const types = globals.__tembraStandard[TYPES] as typeof import("wasi:http/types@0.3.0");
  const fields = (entries: [string, string][]) =>
    types.Fields.fromList(entries.map(([name, value]) => [name, text(value)]));
  /** A JSON host operation; the dependency checks that the callback (its context) is alive. */
  async function json(packet: string): Promise<string> {
    const request = JSON.parse(packet);
    if (request.operation === "wait") {
      await new Promise((resolve) => setTimeout(resolve, request.milliseconds));
      return "null";
    }
    if (!alive) throw new Error("scope has ended");
    ledger.push(request.entry);
    return String(ledger.length);
  }

  /** One handler callback, driven like `BrowserPort.execute`. */
  async function serve(
    exchange: IngressExchange,
    handler: (request: any) => unknown,
    signal?: AbortSignal,
  ): Promise<void> {
    const ended = new AbortController();
    const abort = () => ended.abort();
    signal?.addEventListener("abort", abort, { once: true });
    const request = standard.beginIngress(exchange);
    alive = true;
    let failure: unknown;
    let finished = false;
    const dispatch = (globals.__tembraIngress.serve(request, handler) as Promise<void>).then(
      () => {
        finished = true;
      },
      (error) => {
        failure = error;
        finished = true;
      },
    );
    const pending = new Set<Promise<unknown>>();
    while (!finished) {
      for (const effect of globals.__tembraTakeEffects()) {
        const operation =
          typeof effect.packet === "string"
            ? json(effect.packet)
            : standard.async(effect.packet, ended.signal).then((delivery) => delivery.bytes);
        const completed = operation
          .then(
            (value) => globals.__tembraCompleteEffect(effect.id, true, value),
            (error) => globals.__tembraCompleteEffect(effect.id, false, String(error?.message)),
          )
          .catch(() => {});
        pending.add(completed);
      }
      await tick();
    }
    await dispatch;
    signal?.removeEventListener("abort", abort);
    ended.abort();
    await Promise.allSettled(pending);
    globals.__tembraStopEffects();
    globals.__tembraSweepStandard(true);
    standard.endIngress(failure ?? new Error("the HTTP handler callback ended"));
    standard.endCallback();
    await standard.settled();
    alive = false;
    exchange.settle(failure);
    if (failure) throw failure;
  }

  return {
    standard,
    types,
    fields,
    note,
    wait: (ms: number): Promise<unknown> => globals.tembra.wait(ms),
    ledger,
    serve,
    get alive() {
      return alive;
    },
    async dispose() {
      globals.__tembraSweepStandard(true);
      await standard.dispose();
      expect(standard.size).toBe(0);
      expect(standard.pending).toBe(0);
      expect(standard.inputBytes).toBe(0);
    },
  };
}

function request(body: AsyncIterable<Uint8Array> | null, pathWithQuery = "/"): IncomingHttpRequest {
  return {
    method: "POST",
    scheme: "https",
    authority: "app.example.test",
    pathWithQuery,
    fields: [["x-raw", Uint8Array.of(0x80, 0xff)]],
    body,
  };
}

/** Chunked guest stream sources and reads, as the SDK's `writeChunks`/`readChunks` do. */
const chunks = (generate: () => AsyncGenerator<Uint8Array>) => ({ [WRITE_CHUNKS]: generate });
async function* read(stream: any): AsyncGenerator<Uint8Array> {
  for (let chunk = await stream[READ_CHUNK](); chunk; chunk = await stream[READ_CHUNK]())
    yield chunk;
}

test("SSE: head and bytes before the request ends; post-head and post-transmission authority", async () => {
  const guest = engine();
  const body = source();
  const exchange = new IngressExchange(checkedRequest(request(body.body, "/sse")));
  let reused = "";
  const served = guest.serve(exchange, (incoming) => {
    const [stream] = guest.types.Request.consumeBody(incoming, ok);
    let settle!: (value: unknown) => void;
    const trailers = new Promise<any>((resolve) => {
      settle = resolve;
    });
    let response: any;
    async function* events() {
      yield text("data: 1\n\n");
      let ticks = 0;
      for await (const chunk of read(stream))
        yield text(`data: tick-${++ticks}:${chunk.length}\n\n`);
      // A context-checked dependency call after the head was published.
      const noted = await guest.note(`after-head:${ticks}`);
      // The transferred response is the host's: the guest cannot use it again.
      try {
        response.getStatusCode();
        reused = "reused";
      } catch {
        reused = "transferred";
      }
      yield text(`data: noted-${noted}\n\n`);
      settle({ tag: "ok", val: guest.fields([["x-ticks", String(ticks)]]) });
    }
    const created = guest.types.Response.new(
      guest.fields([["content-type", "text/event-stream"]]),
      chunks(events) as never,
      trailers,
    );
    response = created[0];
    // Work after the transmission result still runs in this callback.
    void created[1].then((result: { tag: string }) => guest.note(`transmitted:${result.tag}`));
    return response;
  });
  const head = await exchange.head;
  expect(head.status).toBe(200);
  const iterator = head.body[Symbol.asyncIterator]();
  expect(decode((await iterator.next()).value)).toBe("data: 1\n\n");
  // The request body is still open; the callback is alive.
  expect(guest.alive).toBe(true);
  body.push("abc");
  expect(decode((await iterator.next()).value)).toBe("data: tick-1:3\n\n");
  body.end();
  // A slow reader loses nothing: the guest waits for the host to take each chunk.
  await new Promise((resolve) => setTimeout(resolve, 30));
  expect(decode((await iterator.next()).value)).toBe("data: noted-1\n\n");
  expect(guest.alive).toBe(true);
  expect((await iterator.next()).done).toBe(true);
  expect(await head.trailers).toEqual([["x-ticks", text("1")]]);
  await served;
  expect(guest.ledger).toEqual(["after-head:1", "transmitted:ok"]);
  expect(reused).toBe("transferred");
  expect(guest.alive).toBe(false);
  await guest.dispose();
});

test("disconnect while writing: the writer stops and the handler learns connection-terminated", async () => {
  const guest = engine();
  const body = source();
  const exchange = new IngressExchange(checkedRequest(request(body.body, "/write")));
  const served = guest.serve(exchange, () => {
    async function* events() {
      for (let n = 0; ; n++) {
        yield text(`data: ${n}\n\n`);
        await guest.wait(5);
      }
    }
    const [response, transmitted] = guest.types.Response.new(
      guest.fields([]),
      chunks(events) as never,
      ok,
    );
    void transmitted.then((result: any) =>
      guest.note(`transmitted:${result.val?.tag ?? result.tag}`),
    );
    return response;
  });
  const head = await exchange.head;
  const iterator = head.body[Symbol.asyncIterator]();
  expect(decode((await iterator.next()).value)).toBe("data: 0\n\n");
  expect(decode((await iterator.next()).value)).toBe("data: 1\n\n");
  await iterator.return!();
  await served;
  expect(guest.ledger).toEqual(["transmitted:connection-terminated"]);
  expect(body.state.returned).toBe(1);
  await guest.dispose();
});

test("disconnect while idle in a request read: the read ends and post-disconnect work runs", async () => {
  const guest = engine();
  const body = source();
  const exchange = new IngressExchange(checkedRequest(request(body.body, "/idle")));
  const served = guest.serve(exchange, (incoming) => {
    const [stream] = guest.types.Request.consumeBody(incoming, ok);
    async function* events() {
      yield text("data: 1\n\n");
      for (const chunks = read(stream); !(await chunks.next()).done;);
      await guest.note("read-ended");
      yield text("data: late\n\n");
    }
    const [response, transmitted] = guest.types.Response.new(
      guest.fields([]),
      chunks(events) as never,
      ok,
    );
    void transmitted.then((result: any) =>
      guest.note(`transmitted:${result.val?.tag ?? result.tag}`),
    );
    return response;
  });
  const head = await exchange.head;
  const iterator = head.body[Symbol.asyncIterator]();
  expect(decode((await iterator.next()).value)).toBe("data: 1\n\n");
  await tick();
  await iterator.return!();
  await served;
  expect([...guest.ledger].sort()).toEqual(["read-ended", "transmitted:connection-terminated"]);
  expect(body.state.returned).toBe(1);
  await guest.dispose();
});

test("an error code is reported after settlement; a trap rejects the request", async () => {
  const guest = engine();
  const refused = new IngressExchange(checkedRequest(request(null, "/error")));
  await guest.serve(refused, () => {
    throw Object.assign(new Error("denied"), { payload: { tag: "HTTP-request-denied" } });
  });
  const error = await refused.head.catch((reason) => reason);
  expect(error).toBeInstanceOf(HttpHandlerError);
  expect(error.code).toEqual({ tag: "HTTP-request-denied" });
  const trapped = new IngressExchange(checkedRequest(request(null, "/trap")));
  const reported = trapped.head.then(
    () => "delivered",
    (error: Error) => error.message,
  );
  await expect(
    guest.serve(trapped, () => {
      throw new Error("handler trap");
    }),
  ).rejects.toThrow("handler trap");
  expect(await reported).toBe("handler trap");
  await guest.dispose();
});

test("cancellation ends a streaming callback and its delivery", async () => {
  const guest = engine();
  const exchange = new IngressExchange(checkedRequest(request(null, "/slow")));
  const cancel = new AbortController();
  const served = guest.serve(
    exchange,
    () => {
      async function* events() {
        yield text("data: 1\n\n");
        for (;;) await guest.wait(10);
      }
      return guest.types.Response.new(guest.fields([]), chunks(events) as never, ok)[0];
    },
    cancel.signal,
  );
  const head = await exchange.head;
  const iterator = head.body[Symbol.asyncIterator]();
  expect(decode((await iterator.next()).value)).toBe("data: 1\n\n");
  const next = iterator.next().then(
    () => "delivered",
    () => "rejected",
  );
  cancel.abort();
  await served.catch(() => {});
  expect(await next).toBe("rejected");
  await guest.dispose();
});
