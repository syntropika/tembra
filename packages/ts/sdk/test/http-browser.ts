import { createHost, fetchHttpProvider } from "../src/browser.ts";

/** Run the authored HTTP Orb through the browser Wasm interpreter and injected Fetch. */
export async function verifyHttpScenario(engine?: URL): Promise<string> {
  const artifact = new Uint8Array(await (await fetch("/http.wasm")).arrayBuffer());
  let sent = 0,
    closed = 0;
  const sentCount = () => sent;
  const host = await createHost({
    isolated: {
      ...(engine ? { engine } : {}),
      providers: {
        http: {
          outbound: fetchHttpProvider(async (input, init) => {
            sent++;
            const url = new URL(String(input));
            if (url.pathname === "/upload") {
              const bytes = init?.body;
              if (!(bytes instanceof Uint8Array))
                throw new Error("Fetch did not receive raw bytes");
              for (let index = 0; index < bytes.length; index++)
                if (bytes[index] !== Math.floor(index / 65536) % 251)
                  throw new Error("Upload byte mismatch");
              return new Response(String(bytes.length));
            }
            if (url.pathname === "/unused")
              return new Response(
                new ReadableStream({
                  start(controller) {
                    controller.enqueue(Uint8Array.of(1));
                  },
                  cancel() {
                    closed++;
                  },
                }),
              );
            return new Response("hé🌍", {
              headers: [
                ["x-repeat", "one"],
                ["x-repeat", "two"],
              ],
            });
          }),
        },
      },
    },
  });
  let requests;
  try {
    await host.install({
      artifact,
      grants: { http: [{ provider: "outbound", origins: ["https://api.example.test"] }] },
    });
    requests = await host.get("http::example:http/requests@1.0.0", "main");
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
    let rejected = false;
    try {
      await requests.upload("https://api.example.test/upload", 4, true);
    } catch {
      rejected = true;
    }
    if (!rejected || sentCount() !== 2) throw new Error("A failed source upload reached Fetch");
    if (
      (await requests.open("https://api.example.test/retained")) !== 200 ||
      (await requests.read()) !== "hé🌍"
    )
      throw new Error("Response did not survive a callback boundary");
    await requests.open("https://api.example.test/unused");
    await requests.close();
    if (closed !== 1) throw new Error("An untouched response was not closed exactly once");
    try {
      await requests.inspect("https://denied.example.test/");
      rejected = false;
    } catch {
      rejected = true;
    }
    if (!rejected || sentCount() !== 4) throw new Error("An ungranted origin reached Fetch");
  } finally {
    requests?.release();
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
    if (diagnostics[name] !== 0) throw new Error(`${name} survived HTTP retirement`);
  return "standard HTTP: 3 MiB byte upload, producer failure, retained response and close";
}

/** A raw host adapter can supply capabilities that Fetch cannot expose. */
export async function verifyRawHttpScenario(engine?: URL): Promise<string> {
  const artifact = new Uint8Array(await (await fetch("/http.wasm")).arrayBuffer());
  const encoder = new TextEncoder();
  let chunks = 0,
    uploadEnded = false,
    closed = 0;
  const host = await createHost({
    isolated: {
      ...(engine ? { engine } : {}),
      providers: {
        http: {
          raw: {
            async send(request) {
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
  let requests;
  try {
    await host.install({
      artifact,
      grants: { http: [{ provider: "raw", origins: ["https://raw.example.test"] }] },
    });
    requests = await host.get("http::example:http/requests@1.0.0", "main");
    if (
      (await requests.duplex("https://raw.example.test/duplex")) !== "response before upload EOF" ||
      chunks !== 1 ||
      uploadEnded ||
      closed !== 1
    )
      throw new Error("Raw provider did not preserve duplex/SSE and physical close");
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
    if (host.diagnostics()[name] !== 0) throw new Error(`${name} survived raw HTTP retirement`);
  return "raw HTTP: duplex/SSE, byte fields and request/response trailers";
}
