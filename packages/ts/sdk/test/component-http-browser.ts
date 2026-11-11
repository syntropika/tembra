/** Operation-level WASI HTTP qualification through real compiled guest bindings in Chrome. */
import {
  createHost,
  fetchHttpProvider,
  HttpProviderError,
  type PreparedComponent,
} from "../src/browser.ts";
import type { Host } from "../src/host.ts";
import type {
  HttpField,
  HttpProvider,
  HttpProviderRequest,
} from "../src/interpreter/http-provider.ts";

const ORIGIN = "https://compiled.example.test";
const FETCHER = "http-client::example:fetch/fetcher@1.0.0";
const encode = (text: string) => new TextEncoder().encode(text);
const decode = (bytes: Uint8Array) => new TextDecoder().decode(Uint8Array.from(bytes));
type Reference = { release(): void; [method: string]: any };
type Field = [string, Uint8Array];
interface Spec {
  method: string;
  scheme: string;
  authority: string;
  path: string;
  fields: Field[];
  chunks: Uint8Array[];
  trailers: Field[];
  trailerError: boolean;
  connect: bigint | undefined;
  firstByte: bigint | undefined;
  betweenBytes: bigint | undefined;
}
interface Report {
  status: number;
  fields: Field[];
  body: Uint8Array;
  trailers: { tag: "ok"; val: Field[] } | { tag: "err"; val: string };
  transmission: { tag: "ok" } | { tag: "err"; val: string };
}

function equal(actual: unknown, expected: unknown, label: string): void {
  const key = (_name: string, value: unknown) =>
    typeof value === "bigint" ? `${value}n` : value instanceof Uint8Array ? [...value] : value;
  if (JSON.stringify(actual, key) !== JSON.stringify(expected, key))
    throw new Error(
      `${label}: expected ${JSON.stringify(expected, key)}, got ${JSON.stringify(actual, key)}`,
    );
}
function includes(actual: string, expected: string, label: string): void {
  if (!actual.includes(expected))
    throw new Error(`${label}: ${actual} does not include ${expected}`);
}
function ok<T>(result: { tag: string; val: T }, label: string): T {
  equal(result.tag, "ok", label);
  return result.val;
}
function error(result: { tag: string; val: unknown }, expected: string): void {
  equal(result.tag, "err", expected);
  includes(String(result.val), expected, "standard error code");
}
function spec(path: string, change: Partial<Spec> = {}): Spec {
  return {
    method: "GET",
    scheme: "https",
    authority: "compiled.example.test",
    path,
    fields: [],
    chunks: [],
    trailers: [],
    trailerError: false,
    connect: undefined,
    firstByte: undefined,
    betweenBytes: undefined,
    ...change,
  };
}
function response(body = "", fields: readonly HttpField[] = [], status = 200) {
  return {
    status,
    fields,
    body: new ReadableStream<Uint8Array>({
      start(controller) {
        if (body) controller.enqueue(encode(body));
        controller.close();
      },
    }),
  };
}
async function artifact(): Promise<Uint8Array> {
  return new Uint8Array(await (await fetch("/http-client.wasm")).arrayBuffer());
}
async function using(
  base: object,
  components: PreparedComponent[],
  provider: HttpProvider,
  run: (host: Host, client: Reference) => Promise<void>,
): Promise<void> {
  const unhandled: unknown[] = [];
  const rejection = (event: PromiseRejectionEvent) => {
    unhandled.push(event.reason);
    event.preventDefault();
  };
  const host = await createHost({
    isolated: { ...base, components, providers: { http: { test: provider } } },
  });
  globalThis.addEventListener("unhandledrejection", rejection);
  let client: Reference | undefined;
  try {
    await host.install({
      artifact: await artifact(),
      grants: { http: [{ provider: "test", origins: [ORIGIN] }] },
    });
    client = (await host.get(FETCHER, "main")) as Reference;
    await run(host, client);
    await new Promise((resolve) => setTimeout(resolve, 0));
    equal(unhandled.length, 0, "no unobserved provider completion failures");
    equal(host.errors().length, 0, "no hidden HTTP failures");
  } finally {
    try {
      client?.release();
      await host.close();
    } finally {
      globalThis.removeEventListener("unhandledrejection", rejection);
    }
    for (const [name, value] of Object.entries(host.diagnostics()))
      if (
        [
          "objects",
          "references",
          "calls",
          "roots",
          "resources",
          "tasks",
          "values",
          "pendingJobs",
        ].includes(name)
      )
        equal(value, 0, `${name} after HTTP close`);
  }
}

/** All synchronous resource methods cross the generated canonical ABI, including exact u64s. */
export async function verifyCompiledHttpValues(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let sends = 0;
  await using(
    base,
    components,
    {
      async send() {
        sends++;
        return response();
      },
    },
    async (_host, client) => {
      const result = ok(await client.values(), "compiled values") as {
        checks: string[];
        duplicateValues: Uint8Array[];
        exactTimeouts: bigint[];
      };
      equal(
        result.checks,
        [
          "fields.new",
          "fields.get",
          "fields.has",
          "fields.duplicates",
          "fields.copy-all",
          "fields.set-and-clone",
          "fields.append",
          "fields.get-and-delete",
          "fields.delete",
          "fields.invalid",
          "fields.forbidden",
          "fields.limit",
          "fields.content-length",
          "options.defaults",
          "options.exact-and-clone",
          "options.reset",
          "request.defaults",
          "request.method",
          "request.scheme",
          "request.authority",
          "request.path",
          "request.immutable-headers",
          "request.immutable-options",
          "request.consume-body",
          "response.default-status",
          "response.status",
          "response.immutable-headers",
        ],
        "every resource check executed",
      );
      equal(result.duplicateValues, [encode("one"), encode("two")], "duplicate header bytes");
      equal(
        result.exactTimeouts,
        [(1n << 53n) + 1n, (1n << 64n) - 2n, (1n << 64n) - 1n],
        "u64 durations survive compiled bindings",
      );
    },
  );
  equal(sends, 0, "value inspection performs no network work");
}

/** Full duplex, bounded pull uploads, exact targets/options, duplicate bytes and both trailers. */
export async function verifyCompiledHttpDuplex(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  const chunks = [
    Uint8Array.from({ length: 70_000 }, (_, index) => index % 251),
    Uint8Array.of(0, 0xff, 0x80),
  ];
  const total = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  let sent = 0;
  let pulls = 0;
  let headPublished = false;
  let uploadEnded = false;
  let uploadTrailers: readonly HttpField[] | undefined;
  let seen: HttpProviderRequest | undefined;
  let settle!: (value: readonly HttpField[]) => void;
  const trailers = new Promise<readonly HttpField[]>((resolve) => (settle = resolve));
  const provider: HttpProvider = {
    async send(request) {
      seen = request;
      equal(pulls, 0, "no upload read-ahead before provider demand");
      const reader = request.body!.getReader();
      const body = new ReadableStream<Uint8Array>(
        {
          async pull(controller) {
            equal(headPublished, true, "response head precedes upload completion");
            const next = await reader.read();
            if (next.done) {
              uploadEnded = true;
              uploadTrailers = await request.trailers;
              settle([
                ["x-final", Uint8Array.of(0xff, 0x81)],
                ["x-final", encode("two")],
              ]);
              controller.close();
            } else {
              pulls++;
              if (next.value.length > 65_536) throw new Error("unbounded compiled upload chunk");
              sent += next.value.length;
              controller.enqueue(next.value);
            }
          },
        },
        { highWaterMark: 0 },
      );
      headPublished = true;
      return {
        status: 207,
        fields: [
          ["x-raw", Uint8Array.of(0xff, 0x80)],
          ["x-dup", encode("one")],
          ["x-dup", encode("two")],
        ],
        body,
        trailers,
      };
    },
  };
  await using(base, components, provider, async (_host, client) => {
    const result = ok(
      await client.exchange(
        spec("/raw/../upload?q=a#fragment", {
          method: "CUSTOM",
          chunks,
          fields: [
            ["x-raw", Uint8Array.of(0xff, 0x80)],
            ["x-dup", encode("one")],
            ["x-dup", encode("two")],
            ["content-length", encode(String(total))],
          ],
          trailers: [["x-end", encode("done")]],
          connect: (1n << 53n) + 1n,
          firstByte: (1n << 64n) - 2n,
          betweenBytes: (1n << 64n) - 1n,
        }),
      ),
      "duplex exchange",
    ) as Report;
    equal(result.status, 207, "response status");
    equal(
      result.fields,
      [
        ["x-raw", Uint8Array.of(0xff, 0x80)],
        ["x-dup", encode("one")],
        ["x-dup", encode("two")],
      ],
      "response raw duplicate fields",
    );
    equal([...result.body], [...chunks[0]!, ...chunks[1]!], "full duplex byte order");
    equal(
      result.trailers,
      {
        tag: "ok",
        val: [
          ["x-final", Uint8Array.of(0xff, 0x81)],
          ["x-final", encode("two")],
        ],
      },
      "response trailers",
    );
    equal(result.transmission.tag, "ok", "request body transmission completed");
    equal(sent, total, "bounded upload byte count");
    equal(uploadEnded, true, "upload reached physical EOF");
    equal(uploadTrailers, [["x-end", encode("done")]], "request trailers");
    equal(seen!.pathWithQuery, "/raw/../upload?q=a", "raw validated target");
    equal(seen!.url.pathname, "/upload", "WHATWG URL is independently normalized");
    equal(seen!.method, "CUSTOM", "extension method preserved");
    equal(
      seen!.timeouts,
      { connect: (1n << 53n) + 1n, firstByte: (1n << 64n) - 2n, betweenBytes: (1n << 64n) - 1n },
      "provider receives exact nanosecond durations",
    );
    equal(seen!.signal.aborted, false, "complete response does not abort provider");
  });
}

/** Provider errors and body framing failures reach the standard result or companion future. */
export async function verifyCompiledHttpFailures(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let calls = 0;
  let cancelled = 0;
  const provider: HttpProvider = {
    async send(request) {
      calls++;
      if (request.pathWithQuery === "/dns")
        throw new HttpProviderError({ tag: "DNS-error", val: { rcode: "NXDOMAIN", infoCode: 7 } });
      if (request.pathWithQuery === "/unknown")
        throw new HttpProviderError({ tag: "invalid-code" } as never);
      if (request.pathWithQuery === "/timeout")
        return new Promise((resolve) =>
          request.signal.addEventListener("abort", () => resolve(response()), { once: true }),
        );
      if (request.pathWithQuery === "/status") return response("", [], 700);
      if (request.pathWithQuery === "/bad-status") return response("", [], 42);
      if (request.pathWithQuery === "/between")
        return {
          status: 200,
          fields: [],
          body: new ReadableStream<Uint8Array>(
            {
              pull: () => new Promise(() => {}),
              cancel() {
                cancelled++;
              },
            },
            { highWaterMark: 0 },
          ),
        };
      if (request.pathWithQuery === "/short")
        return response("abc", [["content-length", encode("4")]]);
      if (request.pathWithQuery === "/long")
        return response("abc", [["content-length", encode("2")]]);
      if (request.pathWithQuery === "/missing")
        return { status: 200, fields: [["content-length", encode("1")]], body: null };
      if (request.pathWithQuery === "/empty")
        return {
          status: 200,
          fields: [],
          body: new ReadableStream<Uint8Array>({
            start(controller) {
              controller.enqueue(new Uint8Array());
              controller.enqueue(encode("after-empty"));
              controller.enqueue(new Uint8Array());
              controller.close();
            },
          }),
        };
      if (request.pathWithQuery === "/trailers")
        return {
          ...response("abc"),
          trailers: Promise.reject(new HttpProviderError({ tag: "connection-terminated" })),
        };
      if (request.pathWithQuery === "/upload-trailers") {
        const reader = request.body!.getReader();
        while (!(await reader.read()).done) {}
        return response("committed");
      }
      if (request.pathWithQuery === "/upload-size") {
        const reader = request.body!.getReader();
        while (!(await reader.read()).done) {}
      }
      return response();
    },
  };
  await using(base, components, provider, async (_host, client) => {
    error(await client.exchange(spec("/dns")), "DnsError");
    error(await client.exchange(spec("/unknown")), "InternalError");
    error(
      await client.exchange(spec("/timeout", { firstByte: 5_000_000n })),
      "ConnectionReadTimeout",
    );
    equal(
      (ok(await client.exchange(spec("/status")), "provider extension status") as Report).status,
      700,
      "received status through 999",
    );
    error(await client.exchange(spec("/bad-status")), "InternalError");
    for (const [path, failure] of [
      ["/short", "HttpResponseIncomplete"],
      ["/long", "HttpResponseBodySize"],
      ["/missing", "HttpResponseIncomplete"],
      ["/trailers", "ConnectionTerminated"],
      ["/between", "ConnectionReadTimeout"],
    ]) {
      const result = ok(
        await client.exchange(spec(path!, path === "/between" ? { betweenBytes: 5_000_000n } : {})),
        path!,
      ) as Report;
      error(result.trailers, failure!);
    }
    equal(cancelled, 1, "between-bytes timeout physically cancels response reader once");
    equal(
      decode((ok(await client.exchange(spec("/empty")), "empty response chunks") as Report).body),
      "after-empty",
      "empty chunks are not EOF",
    );
    error(
      await client.exchange(
        spec("/upload-trailers", { method: "POST", chunks: [encode("abc")], trailerError: true }),
      ),
      "ConnectionWriteTimeout",
    );
    error(
      await client.exchange(
        spec("/upload-size", {
          method: "POST",
          chunks: [encode("abc")],
          fields: [["content-length", encode("2")]],
        }),
      ),
      "HttpRequestBodySize",
    );
    const before = calls;
    error(
      await client.exchange(spec("/denied", { authority: "denied.example.test" })),
      "HttpRequestDenied",
    );
    error(await client.exchange(spec("/unsupported", { scheme: "ftp" })), "HttpProtocolError");
    equal(calls, before, "denied and unsupported targets never reach provider");
    equal(
      ok(await client.fetch("https", "compiled.example.test", "/healthy"), "healthy after errors"),
      "200 ",
      "ordinary protocol errors do not quarantine guest",
    );
  });
}

/** Fetch refuses unsupported capabilities before calling the platform instead of emulating them. */
export async function verifyCompiledHttpFetchLimits(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let fetches = 0;
  const provider = fetchHttpProvider(async () => {
    fetches++;
    return new Response("ok", {
      headers: [
        ["x-dup", "one"],
        ["x-dup", "two"],
      ],
    });
  });
  await using(base, components, provider, async (_host, client) => {
    for (const change of [
      { connect: 1n },
      { method: "CONNECT" },
      { method: "GET", chunks: [encode("x")] },
      { method: "POST", trailers: [["x-end", encode("x")]] as Field[] },
    ])
      error(
        await client.exchange(spec("/unsupported", change)),
        change.method === "CONNECT" ? "HttpRequestMethodInvalid" : "InternalError",
      );
    equal(fetches, 0, "unsupported Fetch requests never reach platform");
    const result = ok(
      await client.exchange(spec("/supported", { method: "POST", chunks: [encode("body")] })),
      "supported buffered Fetch upload",
    ) as Report;
    equal(decode(result.body), "ok", "Fetch response body");
    equal(
      result.fields,
      [
        ["content-type", encode("text/plain;charset=UTF-8")],
        ["x-dup", encode("one, two")],
      ],
      "Fetch combines duplicate response fields",
    );
    equal(result.trailers, { tag: "ok", val: [] }, "Fetch exposes no response trailers");
    equal(fetches, 1, "supported Fetch request reaches platform once");
  });
}

/** Retained responses, early drop and revision replacement physically close provider work. */
export async function verifyCompiledHttpLifetime(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let closed = 0;
  const signals: AbortSignal[] = [];
  const provider: HttpProvider = {
    async send(request) {
      signals.push(request.signal);
      return {
        status: 200,
        fields: [],
        body: new ReadableStream<Uint8Array>(
          {
            start(controller) {
              controller.enqueue(encode("first"));
            },
            cancel() {
              closed++;
            },
          },
          { highWaterMark: 0 },
        ),
      };
    },
  };
  await using(base, components, provider, async (host, client) => {
    equal(
      ok(await client.first("https", "compiled.example.test", "/first"), "first chunk"),
      "first",
      "drop after one chunk",
    );
    equal(closed, 1, "early drop physically closes response once");
    equal(signals[0]!.aborted, true, "early reader drop aborts exchange");
    equal(
      ok(await client.keep("https", "compiled.example.test", "/keep"), "kept response"),
      200,
      "head retained across roots",
    );
    equal(closed, 1, "kept response stays open");
    equal(
      ok(await client["close-kept"](), "close kept"),
      "first",
      "retained body readable in later callback",
    );
    equal(closed, 2, "retained response closed once");
    equal(
      ok(await client.keep("https", "compiled.example.test", "/reload"), "before reload"),
      200,
      "retain before reload",
    );
    const reloaded = await host.reload([
      {
        artifact: await artifact(),
        grants: { http: [{ provider: "test", origins: ["https://replacement.example.test"] }] },
      },
    ]);
    const until = performance.now() + 1000;
    while (closed < 3 && performance.now() < until)
      await new Promise((resolve) => setTimeout(resolve, 1));
    if (closed !== 3)
      throw new Error(
        `HTTP revision not retired: ${JSON.stringify(reloaded)}; diagnostics ${JSON.stringify(host.diagnostics())}`,
      );
    equal(closed, 3, "reload closes old revision's unconsumed response");
    error(
      await client.keep("https", "compiled.example.test", "/no-stale-grant"),
      "HttpRequestDenied",
    );
    equal(signals.length, 3, "replacement does not inherit the old revision's origin grant");
    equal(
      signals.every((signal) => signal.aborted),
      true,
      "every abandoned exchange signal aborted",
    );
  });
}

/** Cancellation stays prompt while late headers and body cancellation remain physical work. */
export async function verifyCompiledHttpCancellation(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  let started!: () => void;
  const ready = new Promise<void>((resolve) => (started = resolve));
  let publish!: () => void;
  let signal: AbortSignal | undefined;
  let lateClosed = 0;
  const provider: HttpProvider = {
    async send(request) {
      if (request.pathWithQuery !== "/late") return response("healthy");
      signal = request.signal;
      started();
      return new Promise(
        (resolve) =>
          (publish = () =>
            resolve({
              status: 200,
              fields: [],
              body: new ReadableStream<Uint8Array>({
                cancel() {
                  lateClosed++;
                },
              }),
              trailers: Promise.reject(new HttpProviderError({ tag: "connection-terminated" })),
            })),
      );
    },
  };
  await using(base, components, provider, async (host, client) => {
    await host.root(async (context) => {
      const pending = client
        .fetch("https", "compiled.example.test", "/late", context)
        .catch((failure: unknown) => failure);
      await ready;
      context.cancel();
      const outcome = await Promise.race([
        pending,
        new Promise((resolve) => setTimeout(() => resolve("timeout"), 100)),
      ]);
      // Release physical work even if the assertion fails, so the qualification cleans itself.
      publish();
      if (!(outcome instanceof Error))
        throw new Error("HTTP cancellation waited for late provider headers");
      equal(signal!.aborted, true, "cancelled callback aborts provider signal");
    });
    const until = performance.now() + 1000;
    while (lateClosed < 1 && performance.now() < until)
      await new Promise((resolve) => setTimeout(resolve, 1));
    equal(lateClosed, 1, "late provider response cancelled exactly once");
    equal(
      ok(
        await client.fetch("https", "compiled.example.test", "/healthy"),
        "healthy after cancellation",
      ),
      "200 healthy",
      "cooperative cancellation does not quarantine guest",
    );
  });

  let reading!: () => void;
  const bodyReady = new Promise<void>((resolve) => (reading = resolve));
  let bodyClosed = 0;
  let bodySignal: AbortSignal | undefined;
  await using(
    base,
    components,
    {
      async send(request) {
        if (request.pathWithQuery !== "/quiet") return response("healthy");
        bodySignal = request.signal;
        return {
          status: 200,
          fields: [],
          body: new ReadableStream<Uint8Array>(
            {
              pull() {
                reading();
                return new Promise(() => {});
              },
              cancel() {
                bodyClosed++;
              },
            },
            { highWaterMark: 0 },
          ),
        };
      },
    },
    async (host, client) => {
      await host.root(async (context) => {
        const pending = client
          .exchange(spec("/quiet"), context)
          .catch((failure: unknown) => failure);
        await bodyReady;
        context.cancel();
        const outcome = await pending;
        if (!(outcome instanceof Error)) throw new Error("Cancelled response read succeeded");
      });
      equal(bodyClosed, 1, "cancelled response read closes body once");
      equal(bodySignal!.aborted, true, "response cancellation aborts transport signal");
      equal(
        ok(
          await client.fetch("https", "compiled.example.test", "/healthy"),
          "healthy after body cancellation",
        ),
        "200 healthy",
        "guest remains reusable after body cancellation",
      );
    },
  );
}
