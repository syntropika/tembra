import { createHost, fetchHttpProvider, HttpHandlerError } from "../src/browser.ts";

const RETIRED = [
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

/** One installation of the authored HTTP Orb with an injected Fetch, retired and checked. */
async function withHttp(
  fetcher: Parameters<typeof fetchHttpProvider>[0],
  run: (requests: any) => Promise<void>,
  timeoutMs?: number,
): Promise<void> {
  const artifact = new Uint8Array(await (await fetch("/http.wasm")).arrayBuffer());
  const host = await createHost({
    isolated: {
      providers: { http: { outbound: fetchHttpProvider(fetcher) } },
      ...(timeoutMs === undefined ? {} : { limits: { timeoutMs } }),
    },
  });
  let requests: any;
  try {
    await host.install({
      artifact,
      grants: {
        http: [{ provider: "outbound", origins: ["https://api.example.test"] }],
      },
    });
    requests = await host.get("http::example:http/requests@1.0.0", "main");
    await run(requests);
  } finally {
    requests?.release();
    await host.close();
  }
  const diagnostics = host.diagnostics();
  for (const name of RETIRED)
    if (diagnostics[name] !== 0) throw new Error(`${name} survived HTTP retirement`);
}

/** A failed non-`result` export traps; the revision then refuses every later call. */
async function trapsAndQuarantines(requests: any, operation: () => Promise<unknown>) {
  let trapped = false;
  try {
    await operation();
  } catch {
    trapped = true;
  }
  if (!trapped) throw new Error("The failing call succeeded");
  let quarantined = false;
  try {
    await requests.read();
  } catch (error) {
    quarantined = /quarantined/.test(String(error));
  }
  if (!quarantined) throw new Error("A trapped revision accepted another call");
}

/** Deadline of the installations whose refusals must settle promptly. */
const PROMPT_LIMIT = 4000;

/**
 * A caught standard refusal is a recoverable `err`: the guest's unread request contents and
 * trailers are released at once, so the callback settles far before its deadline and the revision
 * keeps serving. Returns the elapsed milliseconds.
 */
async function caughtPromptly(
  operation: () => Promise<unknown>,
  expected: unknown,
  what: string,
): Promise<number> {
  const started = performance.now();
  let outcome: unknown;
  try {
    outcome = { ok: await operation() };
  } catch (error) {
    outcome = { failed: String((error as Error)?.message ?? error) };
  }
  const elapsed = performance.now() - started;
  if (JSON.stringify(outcome) !== JSON.stringify(expected))
    throw new Error(
      `${what}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(outcome)}`,
    );
  if (elapsed > PROMPT_LIMIT / 2)
    throw new Error(`${what}: settled only after ${Math.round(elapsed)} ms`);
  return elapsed;
}

/** Run the authored HTTP Orb through browser component execution and injected Fetch. */
export async function verifyHttpScenario(): Promise<string> {
  let sent = 0,
    closed = 0;
  const fetcher: Parameters<typeof fetchHttpProvider>[0] = async (input, init) => {
    sent++;
    const url = new URL(String(input));
    if (url.pathname === "/upload") {
      const bytes = init?.body;
      if (!(bytes instanceof Uint8Array)) throw new Error("Fetch did not receive raw bytes");
      for (let index = 0; index < bytes.length; index++)
        if (bytes[index] !== Math.floor(index / 65536) % 251)
          throw new Error("Upload byte mismatch");
      return new Response(String(bytes.length));
    }
    if (url.pathname === "/unused") {
      let ended = false;
      const finish = () => {
        if (ended) return;
        ended = true;
        closed++;
      };
      return new Response(
        new ReadableStream({
          start(controller) {
            controller.enqueue(Uint8Array.of(1));
            // Match Fetch: aborting the exchange errors its native response body, so a later
            // reader.cancel() rejects with that same abort rather than a destructor failure.
            init?.signal?.addEventListener(
              "abort",
              () => {
                finish();
                controller.error(init.signal?.reason);
              },
              { once: true },
            );
          },
          cancel: finish,
        }),
      );
    }
    return new Response("hé🌍", {
      headers: [
        ["x-repeat", "one"],
        ["x-repeat", "two"],
      ],
    });
  };
  await withHttp(fetcher, async (requests) => {
    const response = (await requests.inspect("https://api.example.test/inspect")) as {
      status: number;
      text: string;
      duplicateCount: number;
      trailer?: string;
    };
    if (
      response.status !== 200 ||
      response.text !== "hé🌍" ||
      response.duplicateCount !== 1 ||
      response.trailer != null
    )
      throw new Error("Fetch response or its documented header/trailer limits differ");
    if (
      (await requests.upload("https://api.example.test/upload", 48, false)) !==
      String(3 * (1 << 20))
    )
      throw new Error("HTTP body did not cross the standard stream boundary");
    if (sent !== 2) throw new Error(`Expected two Fetch calls, saw ${sent}`);
  });
  // Breaking semantics: an ungranted origin and a failing raw upload producer both end their
  // non-`result` export with a trap, which quarantines that revision.
  const beforeDenied = sent;
  await withHttp(fetcher, (requests) =>
    trapsAndQuarantines(requests, () => requests.inspect("https://denied.example.test/")),
  );
  if (sent !== beforeDenied) throw new Error("An ungranted origin reached Fetch");
  // The producer fails after its head was streamed, so the exchange may already have started:
  // the trap and the quarantine are required, not the absence of a Fetch call.
  const beforeFailure = sent;
  await withHttp(fetcher, (requests) =>
    trapsAndQuarantines(requests, () =>
      requests.upload("https://api.example.test/upload", 4, true),
    ),
  );
  if (sent - beforeFailure > 1) throw new Error("A failed upload was sent more than once");
  // A response retained by the environment is read or closed by a later callback.
  const beforeRetained = sent;
  await withHttp(fetcher, async (requests) => {
    if (
      (await requests.open("https://api.example.test/retained")) !== 200 ||
      (await requests.read()) !== "hé🌍"
    )
      throw new Error("Response did not survive a callback boundary");
    await requests.open("https://api.example.test/unused");
    await requests.close();
    if (closed !== 1) throw new Error("An untouched response was not closed exactly once");
    if (
      (await requests.open("https://api.example.test/after-close")) !== 200 ||
      (await requests.read()) !== "hé🌍"
    )
      throw new Error("Dropping an aborted response quarantined its revision");
    if (sent !== beforeRetained + 3)
      throw new Error(
        `Expected three Fetch calls for retained responses, saw ${sent - beforeRetained}`,
      );
  });
  // Breaking-safe standard error lane: a caught refusal settles promptly and the revision keeps
  // serving. Denied GET (trailers only), denied POST (contents and owned trailer fields) and an
  // invalid target never reach Fetch.
  const beforeCaught = sent;
  await withHttp(
    fetcher,
    async (requests) => {
      const denied = (err: string) => ({ ok: { tag: "err", val: err } });
      await caughtPromptly(
        () => requests.attempt("https://denied.example.test/", false),
        denied("HTTP operation failed: HTTP-request-denied"),
        "caught denied GET",
      );
      await caughtPromptly(
        () => requests.attempt("https://denied.example.test/upload", true),
        denied("HTTP operation failed: HTTP-request-denied"),
        "caught denied POST with body and trailers",
      );
      await caughtPromptly(
        () => requests.attempt("https://example.123/", true),
        denied("HTTP operation failed: HTTP-request-denied"),
        "caught invalid target",
      );
      if (sent !== beforeCaught) throw new Error("A refused request reached Fetch");
      await caughtPromptly(
        () => requests.attempt("https://api.example.test/after", false),
        { ok: { tag: "ok", val: "hé🌍" } },
        "next call after caught refusals",
      );
      if (sent !== beforeCaught + 1)
        throw new Error("The call after the refusals did not reach Fetch once");
    },
    PROMPT_LIMIT,
  );
  const ingress = await verifyIngressBuiltRefusal();
  return (
    `${ingress}; ` +
    "standard HTTP: 3 MiB byte upload, retained response and close; denied origin and producer failure trap and quarantine; caught denied GET/POST and invalid target settle promptly and the revision keeps serving"
  );
}

/** A raw host adapter can supply capabilities that Fetch cannot expose. */
export async function verifyRawHttpScenario(): Promise<string> {
  const artifact = new Uint8Array(await (await fetch("/http.wasm")).arrayBuffer());
  const encoder = new TextEncoder();
  let chunks = 0,
    uploadEnded = false,
    closed = 0,
    stopped = 0,
    rejected = 0;
  const host = await createHost({
    isolated: {
      limits: { timeoutMs: PROMPT_LIMIT },
      providers: {
        http: {
          raw: {
            async send(request) {
              if (request.url.pathname === "/reject") {
                // A provider failing before it reads the upload: the host releases the guest's
                // body writer itself, so the caught failure settles promptly.
                if (!request.body) throw new Error("Missing upload stream");
                rejected++;
                throw new Error("raw provider refused before reading");
              }
              if (request.url.pathname === "/stop") {
                // The transport stops reading the upload before it answers. As upstream, that is
                // not a transmission failure: the response still streams to the guest.
                const reader = request.body!.getReader();
                const first = await reader.read();
                if (first.done || first.value[0] !== 1)
                  throw new Error("Missing first upload chunk before the stop");
                await reader.cancel();
                stopped++;
                return {
                  status: 200,
                  fields: [["content-type", encoder.encode("text/event-stream")]],
                  body: new ReadableStream({
                    async start(controller) {
                      await new Promise((resolve) => setTimeout(resolve, 20));
                      controller.enqueue(encoder.encode("data: response after upload stop\n\n"));
                    },
                  }),
                };
              }
              if (request.url.pathname === "/duplex") {
                const reader = request.body!.getReader();
                const first = await reader.read();
                if (first.done || first.value[0] !== 1)
                  throw new Error("Missing first duplex upload chunk");
                chunks++;
                // Deliberately leave the upload unfinished. Response release cancels that read end.
                return {
                  status: 200,
                  fields: [["content-type", encoder.encode("text/event-stream")]],
                  body: new ReadableStream({
                    start(controller) {
                      controller.enqueue(encoder.encode("data: response before upload EOF\n\n"));
                    },
                    async cancel() {
                      closed++;
                      await reader.cancel();
                    },
                  }),
                };
              }
              const raw = request.fields.filter(([name]) => name === "x-raw");
              if (raw.length !== 2 || raw[0]![1][0] !== 255 || raw[1]![1][0] !== 254)
                throw new Error("Raw duplicate request fields were changed");
              const reader = request.body!.getReader();
              const bytes: number[] = [];
              for (let next = await reader.read(); !next.done; next = await reader.read())
                bytes.push(...next.value);
              uploadEnded = true;
              if (bytes.join() !== "1,2,3,4") throw new Error("Request stream bytes differ");
              const trailers = await request.trailers;
              if (
                trailers?.[0]?.[0] !== "x-upload" ||
                new TextDecoder().decode(trailers[0]![1]) !== "complete"
              )
                throw new Error("Raw request trailers were lost");
              return {
                status: 201,
                fields: [
                  ["x-repeat", Uint8Array.of(255)],
                  ["x-repeat", Uint8Array.of(254)],
                ],
                body: new ReadableStream({
                  start(controller) {
                    controller.enqueue(encoder.encode("raw transport"));
                    controller.close();
                  },
                }),
                trailers: Promise.resolve([["x-finished", encoder.encode("complete")]]),
              };
            },
          },
        },
      },
    },
  });
  let requests: any;
  try {
    await host.install({
      artifact,
      grants: {
        http: [{ provider: "raw", origins: ["https://raw.example.test"] }],
      },
    });
    requests = await host.get("http::example:http/requests@1.0.0", "main");
    if (
      (await requests.duplex("https://raw.example.test/duplex")) !== "response before upload EOF" ||
      chunks !== 1 ||
      uploadEnded ||
      closed !== 1
    )
      throw new Error("Raw provider did not preserve duplex/SSE and physical close");
    if (
      (await requests.duplex("https://raw.example.test/stop")) !== "response after upload stop" ||
      stopped !== 1
    )
      throw new Error("A provider that stopped reading the upload failed the exchange");
    await caughtPromptly(
      () => requests.attempt("https://raw.example.test/reject", true),
      { ok: { tag: "err", val: "HTTP operation failed: internal-error" } },
      "caught provider failure before the upload was read",
    );
    if (rejected !== 1) throw new Error("The failing provider was not called once");
    const reply = (await requests.exchange("https://raw.example.test/exchange")) as {
      status: number;
      text: string;
      duplicateCount: number;
      trailer?: string;
    };
    if (
      reply.status !== 201 ||
      reply.text !== "raw transport" ||
      reply.duplicateCount !== 2 ||
      reply.trailer !== "complete" ||
      !uploadEnded
    )
      throw new Error("Raw transport response fields or trailers differ");
  } finally {
    requests?.release();
    await host.close();
  }
  for (const name of RETIRED)
    if (host.diagnostics()[name] !== 0) throw new Error(`${name} survived raw HTTP retirement`);
  return "raw HTTP: duplex/SSE, early upload stop, caught provider failure before reading, byte fields and request/response trailers";
}

/**
 * An incoming-HTTP handler that builds a `Response` (body and trailers), drops it unsent and answers
 * its typed `err`: the dropped body's contents and trailers are released, so the refusal is
 * reported promptly and the revision keeps serving.
 */
export async function verifyIngressBuiltRefusal(): Promise<string> {
  const load = async (name: string) =>
    new Uint8Array(await (await fetch(`/${name}.wasm`)).arrayBuffer());
  const host = await createHost({ isolated: { limits: { timeoutMs: PROMPT_LIMIT } } });
  try {
    await host.install({ artifact: await load("ledger") });
    await host.install({
      artifact: await load("ingress"),
      requires: { "example:ingress/ledger@1.0.0": { orb: "ledger", apiVersion: 1 } },
    });
    const request = (pathWithQuery: string) => ({
      method: "GET",
      scheme: "https",
      authority: "app.example.test",
      pathWithQuery,
      fields: [],
      body: null,
    });
    await caughtPromptly(
      async () => {
        try {
          await host.handleHttp("ingress", request("/built-refuse"));
          return "responded";
        } catch (error) {
          if (!(error instanceof HttpHandlerError)) throw error;
          return error.code;
        }
      },
      { ok: { tag: "HTTP-request-denied" } },
      "handler refusal after a dropped built response",
    );
    await caughtPromptly(
      async () => {
        const response = await host.handleHttp("ingress", request("/revision"));
        let text = "";
        for await (const chunk of response.body) text += new TextDecoder().decode(chunk);
        await response.trailers;
        return /^\d+$/.test(text);
      },
      { ok: true },
      "next request after the refusal",
    );
  } finally {
    await host.close();
  }
  const diagnostics = host.diagnostics();
  for (const name of RETIRED)
    if (diagnostics[name] !== 0) throw new Error(`${name} survived incoming HTTP retirement`);
  return "ingress: a dropped built response is released; its handler's err is prompt and the revision keeps serving";
}
