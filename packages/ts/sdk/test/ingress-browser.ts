// Chrome acceptance for incoming HTTP: the authored `wasm-ingress` fixture through
// `Host.handleHttp`, the shared Wasm interpreter and the browser StandardHost. The application
// injects every request; there is no listener. Requires the `ingress` and `ledger` fixtures
// (`scripts/test-wasm-fixtures.ts --ingress`) and contracts that admit the handler export.
import { createHost, HttpHandlerError, type IncomingHttpRequest } from "../src/browser.ts";
import type { HttpHandlerResponse } from "../src/interpreter/http-ingress.ts";

const ORB = "ingress";
const LEDGER = "ledger::example:ingress/ledger@1.0.0";
const encoder = new TextEncoder();
const decoder = new TextDecoder();

async function fixture(name: string): Promise<Uint8Array> {
  const response = await fetch(`/${name}.wasm`);
  if (!response.ok) throw new Error(`Missing fixture ${name}`);
  return new Uint8Array(await response.arrayBuffer());
}

function same(actual: unknown, expected: unknown, what: string): void {
  if (JSON.stringify(actual) !== JSON.stringify(expected))
    throw new Error(`${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
}

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

function request(
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

async function text(response: HttpHandlerResponse): Promise<string> {
  let value = "";
  for await (const chunk of response.body) value += decoder.decode(chunk, { stream: true });
  return value;
}

async function settled(host: Awaited<ReturnType<typeof createHost>>): Promise<void> {
  for (let tries = 0; host.diagnostics().calls && tries < 500; tries++)
    await new Promise((resolve) => setTimeout(resolve, 10));
  if (host.diagnostics().calls) throw new Error("an incoming HTTP call never settled");
}

/** Serve the authored handler in Chrome and check each required behaviour. */
export async function verifyIngressScenario(engine?: URL): Promise<string> {
  const handler = await fixture("ingress");
  const ledger = await fixture("ledger");
  const requires = { "example:ingress/ledger@1.0.0": { orb: "ledger", apiVersion: 1 } };
  const report: string[] = [];
  const host = await createHost({ isolated: { ...(engine ? { engine } : {}) } });
  try {
    await host.install({ artifact: ledger });
    await host.install({ artifact: handler, requires });
    const entries = async () => {
      const reference = await host.get(LEDGER, "main");
      try {
        return (await reference.entries()) as string[];
      } finally {
        reference.release();
      }
    };

    // SSE: head and first bytes before the request body ended; a dependency call after the
    // head; trailers after the body; the transmission result observed by the handler.
    const body = source();
    const sse = await host.handleHttp(ORB, request("/sse", body.body));
    same([sse.status, decoder.decode(sse.fields[0]![1])], [200, "text/event-stream"], "SSE head");
    const events = sse.body[Symbol.asyncIterator]();
    same(decoder.decode((await events.next()).value), "data: 1\n\n", "first event before EOF");
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
          trailers: Promise.resolve([["x-sum", encoder.encode("200000")]]),
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
      echo.fields.find(([name]) => name === "x-seen")?.[1] &&
        decoder.decode(echo.fields.find(([name]) => name === "x-seen")![1]),
      "2",
      "raw duplicate request fields",
    );
    const echoed: Uint8Array[] = [];
    for await (const chunk of echo.body) echoed.push(chunk);
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
      (await echo.trailers)?.map(([name, value]) => [name, decoder.decode(value)]),
      [["x-sum", "200000"]],
      "echo trailers",
    );
    await settled(host);
    report.push("binary request and response bodies, raw duplicate fields and trailers both ways");

    // Errors before the head.
    const refused = await host.handleHttp(ORB, request("/error")).catch((error) => error);
    same(
      [refused instanceof HttpHandlerError, refused.code],
      [true, { tag: "HTTP-request-denied" }],
      "handler error-code",
    );
    const trapped = await host.handleHttp(ORB, request("/trap")).catch((error) => error);
    same(trapped instanceof Error && !(trapped instanceof HttpHandlerError), true, "handler trap");
    report.push("handler error-code and trap reported after settlement");

    // Disconnects: while idle in a request read, and while writing.
    const idleBody = source();
    const idle = await host.handleHttp(ORB, request("/idle", idleBody.body));
    const idleEvents = idle.body[Symbol.asyncIterator]();
    await idleEvents.next();
    await idleEvents.return!();
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
    const fetched = await host.handleHttp(ORB, new Request("https://app.example.test/revision"));
    same([fetched.status, /^\d+$/.test(await fetched.text())], [200, true], "WHATWG response");
    const early = await host
      .handleHttp(ORB, new Request("https://app.example.test/status/103"))
      .catch((error) => String(error));
    same(String(early).includes("103"), true, "1xx refused");
    const cookie = await host
      .handleHttp(ORB, new Request("https://app.example.test/cookie"))
      .catch((error) => String(error));
    same(String(cookie).includes("set-cookie"), true, "set-cookie refused by the browser");
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
  } finally {
    await host.close();
  }
  const diagnostics = host.diagnostics();
  for (const name of [
    "objects",
    "references",
    "roots",
    "calls",
    "resources",
    "tasks",
    "orbs",
    "values",
    "pendingJobs",
  ] as const)
    if (diagnostics[name] !== 0) throw new Error(`${name} survived incoming HTTP retirement`);

  // Timeout covers the body and is terminal for the revision.
  const limited = await createHost({
    isolated: { ...(engine ? { engine } : {}), limits: { timeoutMs: 1500 } },
  });
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
