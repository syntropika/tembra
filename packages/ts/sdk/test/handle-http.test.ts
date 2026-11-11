// `Host.handleHttp` through the real kernel, the SDK root and the isolated `Revision`: the head
// resolves early while the root (and the handler's context authority) lives until the physical
// call ended. The contract answer and the interpreter port are test doubles at the backend seam;
// incoming-HTTP semantics of the browser host are covered by the Chrome `ingress-browser.ts` scenario and the
// real guest component path by the same Chrome scenario.
import { expect, test } from "bun:test";
import { isolate } from "#tembra-adapters";
import { createHost } from "../src/node.ts";
import {
  CONNECTION_TERMINATED,
  HttpHandlerError,
  type HttpHandlerResponse,
  type IngressExchange,
} from "../src/interpreter/http-ingress.ts";
import { Revision } from "../src/interpreter/revision.ts";
import type {
  ContractPort,
  ExecutionScope,
  IngressDispatch,
  InstallationPlan,
  InterpreterPort,
} from "../src/interpreter/types.ts";

const HANDLER = "wasi:http/handler@0.3.0";
const text = (value: string) => new TextEncoder().encode(value);
const decoder = new TextDecoder();

/** The contract module's answers for an Orb whose only export is the HTTP handler. */
const contract: ContractPort = {
  bind: () => 0,
  validate: () => JSON.stringify({ ok: null }),
  request(packet) {
    const query = JSON.parse(packet);
    if (query.kind === "describe")
      return JSON.stringify({
        ok: {
          imports: [],
          exports: [],
          types: [],
          javascriptNames: { handler: "handler", handle: "handle" },
        },
      });
    return JSON.stringify({
      ok: {
        services: [],
        hooks: { hooks: [], contributions: [] },
        plan: { orb: plan.orb, ingress: HANDLER },
      },
    });
  },
};
const plan: InstallationPlan["plan"] = {
  orb: { name: "edge", apiVersion: 1, services: [], requires: [], hooks: [], hookSignatures: {} },
};

type Serve = (
  dispatch: IngressDispatch,
  exchange: IngressExchange,
  scope: ExecutionScope,
) => Promise<void>;

/** A port whose handler callback the test scripts. */
function port(serve?: Serve): InterpreterPort & { entered: number } {
  const value = {
    entered: 0,
    description: () => JSON.stringify({ services: { handler: { methods: ["handle"] } } }),
    declare: () => null,
    execute: () => Promise.reject(new Error("unexpected call")),
    release() {},
    close() {},
    ...(serve
      ? {
          ingress: (
            dispatch: IngressDispatch,
            exchange: IngressExchange,
            scope: ExecutionScope,
          ) => {
            value.entered++;
            return serve(dispatch, exchange, scope);
          },
        }
      : {}),
  };
  return value;
}

async function host(backend: InterpreterPort) {
  const created = createHost({
    principal: "test",
    ...{
      [isolate]: async () =>
        new Revision(backend, contract, 5000).definition(
          { name: "edge", apiVersion: 1 },
          { bindings: {}, importServices: {}, plan },
        ),
    },
  });
  await created.install({ artifact: new Uint8Array() });
  return created;
}

/** A streamed response whose body the test feeds; `ended` settles when its delivery ended. */
function streamed(status = 200, trailers?: [string, Uint8Array][]) {
  const chunks: Uint8Array[] = [];
  let finish = false;
  let wake: (() => void) | undefined;
  let ended!: (how: string) => void;
  const delivery = new Promise<string>((resolve) => {
    ended = resolve;
  });
  const response: HttpHandlerResponse = {
    status,
    fields: [["content-type", text("text/event-stream")]],
    body: {
      [Symbol.asyncIterator]: () => ({
        async next() {
          while (!chunks.length && !finish)
            await new Promise<void>((resolve) => {
              wake = resolve;
            });
          const value = chunks.shift();
          if (value) return { done: false, value };
          ended("eof");
          return { done: true, value: undefined };
        },
        async return() {
          ended("disconnect");
          return { done: true, value: undefined };
        },
      }),
    },
    trailers: Promise.resolve(trailers),
  };
  return {
    response,
    delivery,
    write(value: string) {
      chunks.push(text(value));
      wake?.();
    },
    end() {
      finish = true;
      wake?.();
    },
  };
}

const request = (pathWithQuery = "/") => ({
  method: "GET",
  scheme: "https",
  authority: "app.example.test",
  pathWithQuery,
  fields: [],
  body: null,
});

test("the head resolves early; the root keeps the handler's authority until physical completion", async () => {
  const stream = streamed();
  const checks: string[] = [];
  let retained: ExecutionScope | undefined;
  const backend = port(async (dispatch, exchange, scope) => {
    expect(dispatch).toMatchObject({ service: "handler", method: "handle" });
    expect(exchange.request.pathWithQuery).toBe("/events");
    exchange.respond(stream.response);
    stream.write("data: 1\n\n");
    // Context-checked work after the head was published, while the body still streams.
    await new Promise((resolve) => setTimeout(resolve, 20));
    scope.check();
    checks.push("after-head");
    stream.write("data: 2\n\n");
    stream.end();
    await stream.delivery;
    // And after the transmission ended.
    scope.check();
    checks.push("after-delivery");
    retained = scope;
  });
  const served = await host(backend);
  try {
    // The application holds only the head: no completion observer exists or is awaited.
    const head = await served.handleHttp("edge", request("/events"));
    expect(head.status).toBe(200);
    expect(checks).toEqual([]);
    let body = "";
    for await (const chunk of head.body) body += decoder.decode(chunk);
    expect(body).toBe("data: 1\n\ndata: 2\n\n");
    while (served.diagnostics().calls) await new Promise((resolve) => setTimeout(resolve, 5));
    expect(checks).toEqual(["after-head", "after-delivery"]);
    // The root ended with the physical call: a retained scope has no authority left.
    expect(() => retained!.check()).toThrow();
  } finally {
    await served.close();
  }
  const diagnostics = served.diagnostics();
  for (const name of ["objects", "references", "roots", "calls", "values", "pendingJobs"] as const)
    expect(diagnostics[name]).toBe(0);
});

test("an error before the head is reported only after the call settled", async () => {
  const order: string[] = [];
  const backend = port(async (_dispatch, exchange) => {
    exchange.refuse({ tag: "HTTP-request-denied" });
    await new Promise((resolve) => setTimeout(resolve, 20));
    order.push("settled");
  });
  const served = await host(backend);
  try {
    const error = await served.handleHttp("edge", request()).catch((reason) => {
      order.push("rejected");
      return reason;
    });
    expect(error).toBeInstanceOf(HttpHandlerError);
    expect(error.code).toEqual({ tag: "HTTP-request-denied" });
    expect(order).toEqual(["settled", "rejected"]);
  } finally {
    await served.close();
  }
});

test("options.signal cancels the call: the handler sees the root's cancellation", async () => {
  const stream = streamed();
  let observed = "";
  const backend = port(async (_dispatch, exchange, scope) => {
    exchange.respond(stream.response);
    await new Promise<void>((resolve) =>
      scope.signal!.addEventListener("abort", () => resolve(), { once: true }),
    );
    observed = "cancelled";
  });
  const served = await host(backend);
  try {
    const cancel = new AbortController();
    const head = await served.handleHttp("edge", request(), { signal: cancel.signal });
    expect(head.status).toBe(200);
    cancel.abort();
    while (served.diagnostics().calls) await new Promise((resolve) => setTimeout(resolve, 5));
    expect(observed).toBe("cancelled");
  } finally {
    await served.close();
  }
});

test("the handler service is host-local: a direct call never enters it", async () => {
  const backend = port(async () => {});
  const served = await host(backend);
  try {
    const reference = await served.get(`edge::${HANDLER}`, "main");
    await expect(
      (reference as unknown as { handle(value: unknown): Promise<unknown> }).handle({
        request: request(),
      }),
    ).rejects.toMatchObject({ reason: "local_value" });
    expect(backend.entered).toBe(0);
    reference.release();
  } finally {
    await served.close();
  }
});

test("a host without incoming HTTP refuses after settlement instead of misbehaving", async () => {
  const served = await host(port());
  try {
    await expect(served.handleHttp("edge", request())).rejects.toThrow(
      "does not serve incoming HTTP",
    );
  } finally {
    await served.close();
  }
});

test("WHATWG convenience: a Request gets a Response; unrepresentable responses are refused", async () => {
  const cases = new Map<string, () => ReturnType<typeof streamed>>([
    ["/plain", () => streamed(200)],
    ["/early", () => streamed(103)],
    ["/trailers", () => streamed(200, [["x-late", text("1")]])],
  ]);
  const backend = port(async (_dispatch, exchange) => {
    const stream = cases.get(exchange.request.pathWithQuery!)!();
    exchange.respond(stream.response);
    stream.write("body");
    stream.end();
    await stream.delivery;
  });
  const served = await host(backend);
  try {
    const plain = await served.handleHttp(
      "edge",
      new Request("https://app.example.test/plain", { headers: { "x-raw": "ÿ" } }),
    );
    expect(plain).toBeInstanceOf(Response);
    expect(plain.headers.get("content-type")).toBe("text/event-stream");
    expect(await plain.text()).toBe("body");
    await expect(
      served.handleHttp("edge", new Request("https://app.example.test/early")),
    ).rejects.toThrow("HTTP status 103 has no WHATWG Response");
    const trailed = await served.handleHttp(
      "edge",
      new Request("https://app.example.test/trailers"),
    );
    await expect(trailed.text()).rejects.toThrow("trailers have no WHATWG representation");
    while (served.diagnostics().calls) await new Promise((resolve) => setTimeout(resolve, 5));
  } finally {
    await served.close();
  }
  void CONNECTION_TERMINATED;
});

/** A request body that counts the host's factory and `return()` calls. */
function counted(options: { locked?: boolean } = {}) {
  const state = { factories: 0, returns: 0 };
  const body: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]() {
      state.factories++;
      if (options.locked) throw new TypeError("ReadableStream is locked");
      return {
        next: () => new Promise<IteratorResult<Uint8Array>>(() => {}),
        async return() {
          state.returns++;
          return { done: true, value: undefined };
        },
      };
    },
  };
  return { state, body };
}

test("a request failing before any port took it gets its body closed once (L-1)", async () => {
  const settled = async (served: Awaited<ReturnType<typeof host>>) => {
    while (served.diagnostics().calls) await new Promise((resolve) => setTimeout(resolve, 5));
    await new Promise((resolve) => setTimeout(resolve, 5));
  };
  // No incoming HTTP on this port, and an unknown Orb: the call fails before any port.
  const refusing = await host(port());
  try {
    for (const [orb, refusal] of [
      ["edge", "does not serve incoming HTTP"],
      ["missing", ""],
    ] as const) {
      const body = counted();
      const error = await refusing
        .handleHttp(orb, { ...request(), body: body.body })
        .catch((reason) => reason);
      expect(String(error)).toContain(refusal);
      await settled(refusing);
      expect(body.state).toEqual({ factories: 1, returns: 1 });
      // A body that cannot even be opened keeps the original refusal and escapes nothing.
      const locked = counted({ locked: true });
      const lockedError = await refusing
        .handleHttp(orb, { ...request(), body: locked.body })
        .catch((reason) => reason);
      expect(String(lockedError)).toContain(refusal);
      await settled(refusing);
      expect(locked.state.factories).toBe(1);
    }
  } finally {
    await refusing.close();
  }
  // A port that took the request owns its body: settlement never opens or closes it again.
  const owner = await host(
    port(async (_dispatch, exchange) => {
      exchange.own();
      exchange.refuse({ tag: "HTTP-request-denied" });
    }),
  );
  try {
    const body = counted();
    await expect(owner.handleHttp("edge", { ...request(), body: body.body })).rejects.toThrow(
      HttpHandlerError,
    );
    await settled(owner);
    expect(body.state).toEqual({ factories: 0, returns: 0 });
  } finally {
    await owner.close();
  }
});

test.serial("unavailable WHATWG request streams are refused before the handler runs", async () => {
  const originalStream = globalThis.ReadableStream;
  const requestDescriptor = Object.getOwnPropertyDescriptor(globalThis, "Request")!;
  const streamDescriptor = Object.getOwnPropertyDescriptor(globalThis, "ReadableStream")!;
  // React Native's buffered fetch implementation has no public body stream.
  class BufferedRequest {
    readonly url = "https://app.example.test/echo";
    readonly method = "POST";
    readonly headers = new Headers();
    readonly _bodyInit = "abc";
  }
  const received: string[] = [];
  const backend = port(async (_dispatch, exchange) => {
    const incoming = exchange.own();
    if (incoming.body)
      for await (const chunk of incoming.body) received.push(decoder.decode(chunk));
    const stream = streamed(204);
    stream.end();
    exchange.respond(stream.response);
    await stream.delivery;
  });
  const served = await host(backend);
  try {
    Object.defineProperty(globalThis, "Request", { configurable: true, value: BufferedRequest });
    for (const streams of [undefined, originalStream]) {
      Object.defineProperty(globalThis, "ReadableStream", {
        configurable: true,
        value: streams,
      });
      const result = await served
        .handleHttp("edge", new BufferedRequest() as unknown as Request)
        .then(
          () => undefined,
          (error: unknown) => error,
        );
      expect(result).toBeInstanceOf(TypeError);
      expect(String(result)).toContain("IncomingHttpRequest");
      expect(backend.entered).toBe(0);
      expect(served.diagnostics().roots).toBe(0);
    }
    // The raw form remains usable without any WHATWG stream globals.
    Object.defineProperty(globalThis, "ReadableStream", { configurable: true, value: undefined });
    const response = await served.handleHttp("edge", {
      ...request(),
      method: "POST",
      body: {
        async *[Symbol.asyncIterator]() {
          yield text("abc");
        },
      },
    });
    for await (const chunk of response.body) expect(chunk.length).toBe(0);
    await response.trailers;
    expect(received).toEqual(["abc"]);
    expect(backend.entered).toBe(1);
  } finally {
    Object.defineProperty(globalThis, "Request", requestDescriptor);
    Object.defineProperty(globalThis, "ReadableStream", streamDescriptor);
    await served.close();
  }
});
