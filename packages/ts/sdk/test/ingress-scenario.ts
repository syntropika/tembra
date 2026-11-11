// Shared real-artifact scenario for `Host.handleHttp` with the authored `wasm-ingress` source
// fixture (`ingress` + `ledger`): the platform-neutral form of `ingress-browser.ts`, run on
// Node through the native typed carrier. The application injects every request; there is no
// listener. Bytes are compared as bytes; nothing is JSON- or base64-encoded on the way.
import type { Host } from "../src/host.ts";
import type { HttpHandlerResponse, IncomingHttpRequest } from "../src/interpreter/http-ingress.ts";

export const INGRESS_ORB = "ingress";
const LEDGER = "ledger::example:ingress/ledger@1.0.0";
const encoder = new TextEncoder();
const decoder = new TextDecoder();

export interface IngressScenarioOptions {
  /** A host with the platform's isolated execution; `timeoutMs` bounds each callback. */
  open(limits?: { timeoutMs?: number }): Host | Promise<Host>;
  fixture(name: string): Promise<Uint8Array>;
  /** How this platform's WHATWG `Response` treats `set-cookie` (browsers hide it). */
  setCookie: "kept" | "refused";
  /**
   * Exercise the WHATWG `Request`/`Response` convenience block (default `true`). Hosts without
   * WHATWG streams set `false`; the report then states that block was not exercised.
   */
  whatwg?: boolean;
}

export function same(actual: unknown, expected: unknown, what: string): void {
  const encode = (value: unknown) =>
    JSON.stringify(value, (_, item) => (item instanceof Uint8Array ? [...item] : item));
  if (encode(actual) !== encode(expected))
    throw new Error(`${what}: expected ${encode(expected)}, got ${encode(actual)}`);
}

/** A request body the test feeds; `returned` counts the host's `return()` calls. */
export function source() {
  const queue: Uint8Array[] = [];
  let ended = false;
  let wake: (() => void) | undefined;
  const state = { returned: 0, reads: 0 };
  const body: AsyncIterable<Uint8Array> = {
    [Symbol.asyncIterator]: () => ({
      async next() {
        state.reads++;
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
    push(bytes: Uint8Array) {
      queue.push(bytes);
      wake?.();
    },
    end() {
      ended = true;
      wake?.();
    },
  };
}

export function request(
  pathWithQuery: string,
  body: AsyncIterable<Uint8Array> | null = null,
  extra: Partial<IncomingHttpRequest> = {},
): IncomingHttpRequest {
  return {
    method: "POST",
    scheme: "https",
    authority: "app.example.test",
    pathWithQuery,
    fields: [],
    body,
    ...extra,
  };
}

export async function text(response: HttpHandlerResponse): Promise<string> {
  let value = "";
  for await (const chunk of response.body) value += decoder.decode(chunk, { stream: true });
  return value;
}

/**
 * After a failed handler call the kernel retires that service instance; until it finished
 * closing, a new request is refused with reason `disposing`. Wait for the replacement.
 */
export async function replaced(host: Host, orb: string, probe = "/status/204"): Promise<void> {
  for (let tries = 0; tries < 200; tries++) {
    const outcome = await host.handleHttp(orb, request(probe)).then(
      async (response) => {
        await response.body[Symbol.asyncIterator]().next();
        return "served";
      },
      (error: { reason?: string; message?: string }) =>
        error?.reason === "disposing" || String(error?.message).includes("still closing")
          ? "closing"
          : "failed",
    );
    if (outcome !== "closing") return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error("the failed handler instance was never replaced");
}

export async function settled(host: Host): Promise<void> {
  for (let tries = 0; host.diagnostics().calls && tries < 500; tries++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  if (host.diagnostics().calls) throw new Error("an incoming HTTP call never settled");
}

/** Kernel counters that must all return to zero once the host closed. */
export const COUNTERS = [
  "objects",
  "references",
  "roots",
  "calls",
  "resources",
  "tasks",
  "orbs",
  "values",
  "pendingJobs",
] as const;

/** Serve the authored handler and check each required behaviour; returns a report. */
export async function verifyIngressScenario(options: IngressScenarioOptions): Promise<string> {
  const report: string[] = [];
  try {
    return await scenario(options, report);
  } catch (error) {
    throw new Error(`incoming HTTP failed after [${report.join("; ")}]: ${String(error)}`, {
      cause: error,
    });
  }
}

async function scenario(options: IngressScenarioOptions, report: string[]): Promise<string> {
  const handler = await options.fixture("ingress");
  const ledger = await options.fixture("ledger");
  const requires = { "example:ingress/ledger@1.0.0": { orb: "ledger", apiVersion: 1 } };
  const ORB = INGRESS_ORB;
  const host = await options.open();
  try {
    await host.install({ artifact: ledger });
    await host.install({ artifact: handler, requires });
    const entries = async () => {
      const reference = (await host.get(LEDGER, "main")) as unknown as {
        entries(): Promise<string[]>;
        release(): void;
      };
      try {
        return await reference.entries();
      } finally {
        reference.release();
        // The released ledger may now close; the next request waits for its replacement.
        await replaced(host, ORB);
      }
    };

    // SSE: head and first bytes before the request body ended; a dependency call after the
    // head; trailers after the body; the transmission result observed by the handler.
    const body = source();
    const sse = await host.handleHttp(ORB, request("/sse", body.body));
    same([sse.status, decoder.decode(sse.fields[0]![1])], [200, "text/event-stream"], "SSE head");
    const events = sse.body[Symbol.asyncIterator]();
    same(decoder.decode((await events.next()).value), "data: 1\n\n", "first event before EOF");
    same(body.state.reads <= 1, true, "the request body is pulled only as the handler reads");
    body.push(Uint8Array.of(0, 1, 2));
    same(decoder.decode((await events.next()).value), "data: tick-1:3\n\n", "streamed request");
    body.end();
    same(decoder.decode((await events.next()).value), "data: noted-1\n\n", "post-head authority");
    same((await events.next()).done, true, "SSE end");
    same(
      (await sse.trailers)?.map(([name, value]) => [name, decoder.decode(value)]),
      [["x-ticks", "1"]],
      "SSE trailers",
    );
    await settled(host);
    same(await entries(), ["sse-after-head:1", "sse-transmitted:ok"], "ledger after SSE");
    report.push("SSE head and bytes before request EOF, post-head and post-transmission authority");

    // Binary request and response fields, body and trailers, raw and duplicated.
    const upload = Uint8Array.from({ length: 200_000 }, (_, index) => (index * 7) % 256);
    const echo = await host.handleHttp(
      ORB,
      request(
        "/echo",
        (async function* () {
          yield upload.subarray(0, 70_000);
          yield upload.subarray(70_000);
        })(),
        {
          method: "PATCH",
          fields: [
            ["x-raw", Uint8Array.of(0x80, 0xff)],
            ["x-raw", Uint8Array.of(0x21)],
          ],
          trailers: Promise.resolve([["x-sum", Uint8Array.of(0x32, 0x80, 0xff)]]),
        },
      ),
    );
    same(echo.status, 201, "echo status");
    same(
      echo.fields.filter(([name]) => name === "x-raw").map(([, value]) => [...value]),
      [[0x80, 0xff], [0x21]],
      "raw duplicate response fields",
    );
    same(
      decoder.decode(echo.fields.find(([name]) => name === "x-seen")?.[1]),
      "2",
      "raw duplicate request fields",
    );
    same(
      echo.fields.every(([, value]) => value instanceof Uint8Array),
      true,
      "field values are bytes",
    );
    const echoed: Uint8Array[] = [];
    for await (const chunk of echo.body) {
      if (!(chunk instanceof Uint8Array)) throw new Error("response chunks are Uint8Array");
      echoed.push(chunk);
    }
    const joined = new Uint8Array(echoed.reduce((total, chunk) => total + chunk.length, 0));
    let offset = 0;
    for (const chunk of echoed) {
      joined.set(chunk, offset);
      offset += chunk.length;
    }
    same(
      joined.length === upload.length && joined.every((v, i) => v === upload[i]),
      true,
      "echo body",
    );
    same(
      (await echo.trailers)?.map(([name, value]) => [name, [...value]]),
      [["x-sum", [0x32, 0x80, 0xff]]],
      "raw echo trailers",
    );
    await settled(host);
    report.push("binary request and response bodies, raw duplicate fields and trailers both ways");

    // Errors before the head.
    const refused = await host.handleHttp(ORB, request("/error")).catch((error) => error);
    same(
      [refused?.name, refused?.code],
      ["HttpHandlerError", { tag: "HTTP-request-denied" }],
      "handler error-code",
    );
    await settled(host);
    await replaced(host, ORB);
    const trapped = await host.handleHttp(ORB, request("/trap")).catch((error) => error);
    same(
      trapped instanceof Error && trapped.name !== "HttpHandlerError",
      true,
      `handler trap (${String(trapped)})`,
    );
    await settled(host);
    await replaced(host, ORB);
    report.push("handler error-code and trap reported after settlement");
    report.push("(disconnects)");

    // Disconnects: while idle in a request read, and while writing.
    const idleBody = source();
    const idle = await host.handleHttp(ORB, request("/idle", idleBody.body));
    const idleEvents = idle.body[Symbol.asyncIterator]();
    await idleEvents.next();
    await idleEvents.return!();
    same(
      await idle.trailers.then(
        () => "resolved",
        (error) => error?.code?.tag,
      ),
      "connection-terminated",
      "disconnected trailers",
    );
    await settled(host);
    same(idleBody.state.returned, 1, "idle request body returned");
    const writing = await host.handleHttp(ORB, request("/write"));
    const writeEvents = writing.body[Symbol.asyncIterator]();
    await writeEvents.next();
    await writeEvents.next();
    await writeEvents.return!();
    await settled(host);
    same(
      (await entries()).slice(-3).sort(),
      ["idle-read-ended", "idle-transmitted:err", "write-transmitted:err"],
      "disconnect signals reached the handler",
    );
    report.pop();
    report.push("disconnect while idle in a request read and while writing");

    // Cancellation through the caller's signal.
    const cancel = new AbortController();
    const slow = await host.handleHttp(ORB, request("/slow"), { signal: cancel.signal });
    const slowEvents = slow.body[Symbol.asyncIterator]();
    await slowEvents.next();
    const pending = slowEvents.next().then(
      () => "delivered",
      () => "rejected",
    );
    cancel.abort();
    same(await pending, "rejected", "cancelled delivery");
    same(
      await slow.trailers.then(
        () => "resolved",
        () => "rejected",
      ),
      "rejected",
      "cancelled trailers",
    );
    await settled(host);
    report.push("cancellation through the caller's signal");

    // Reload: an in-flight stream drains on the old revision; new requests reach the new one.
    const before = Number(await text(await host.handleHttp(ORB, request("/revision"))));
    same(before > 1, true, "the first revision served earlier requests");
    const drainingBody = source();
    const draining = await host.handleHttp(ORB, request("/sse", drainingBody.body));
    const drainingEvents = draining.body[Symbol.asyncIterator]();
    await drainingEvents.next();
    const reloading = host.reload([{ artifact: handler, requires }]);
    drainingBody.end();
    let rest = "";
    for (let next = await drainingEvents.next(); !next.done; next = await drainingEvents.next())
      rest += decoder.decode(next.value);
    same(rest.startsWith("data: noted-"), true, "old revision drained");
    await reloading;
    same(await text(await host.handleHttp(ORB, request("/revision"))), "1", "new revision");
    report.push("reload drains the in-flight stream and routes new requests to the new revision");

    // Handler-only Orbs have no outgoing HTTP; the local service refuses direct access.
    same(await text(await host.handleHttp(ORB, request("/outbound"))), "absent", "outbound");
    const direct = await host.get(`${ORB}::wasi:http/handler@0.3.0`, "main");
    const forged = await (direct as unknown as { handle(value: unknown): Promise<unknown> })
      .handle(request("/sse"))
      .catch((error: { reason?: string }) => error);
    direct.release();
    same((forged as { reason?: string } | undefined)?.reason, "local_value", "direct access");
    report.push("handler-only outbound denied; direct handler access refused");

    // WHATWG convenience: representable responses convert, the rest is refused.
    if (options.whatwg === false)
      report.push("WHATWG Request/Response convenience not exercised on this host");
    else {
      const fetched = await host.handleHttp(ORB, new Request("https://app.example.test/revision"));
      same([fetched.status, /^\d+$/.test(await fetched.text())], [200, true], "WHATWG response");
      const early = await host
        .handleHttp(ORB, new Request("https://app.example.test/status/103"))
        .catch((error) => String(error));
      same(String(early).includes("103"), true, "1xx refused");
      const cookie = await host
        .handleHttp(ORB, new Request("https://app.example.test/cookie"))
        .then(
          async (response) => `kept:${response.headers.get("set-cookie")}:${await response.text()}`,
          (error) => String(error),
        );
      if (options.setCookie === "kept") same(cookie, "kept:a=1:x", "set-cookie kept");
      else same(cookie.includes("set-cookie"), true, "set-cookie refused by the platform");
      const trailed = await host.handleHttp(ORB, new Request("https://app.example.test/trailers"));
      same(
        await trailed.text().then(
          () => "read",
          (error) => String(error),
        ),
        "TypeError: response trailers have no WHATWG representation",
        "trailers refused",
      );
      await settled(host);
      report.push("WHATWG Request/Response convenience with explicit refusals");
    }
  } finally {
    await host.close();
  }
  const diagnostics = host.diagnostics() as unknown as Record<string, unknown>;
  for (const name of COUNTERS)
    if (diagnostics[name] !== 0) throw new Error(`${name} survived incoming HTTP retirement`);

  // Timeout covers the body and is terminal for the revision.
  const limited = await options.open({ timeoutMs: 1500 });
  try {
    await limited.install({ artifact: ledger });
    await limited.install({ artifact: handler, requires });
    const slow = await limited.handleHttp(ORB, request("/slow"));
    const outcome = await text(slow).then(
      () => "completed",
      () => "timed out",
    );
    same(outcome, "timed out", "timeout covers the body");
    await settled(limited);
  } finally {
    await limited.close();
  }
  report.push("timeout covers the streamed body and retires the revision");
  return `incoming HTTP: ${report.join("; ")}`;
}

export { encoder, decoder };
