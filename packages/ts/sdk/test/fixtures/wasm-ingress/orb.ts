import { orb, readChunks, writeChunks } from "@syntropika/tembra-sdk";
import type { Context, Declaration } from "@syntropika/tembra-sdk";
import { Fields, Request, Response } from "wasi:http/types@0.3.0";
import type { ExampleIngressLedger100 as Ledger } from "./contract.wit";

/** ASCII text as bytes (the guest has no TextEncoder). */
const text = (value: string) => Uint8Array.from(value, (unit) => unit.charCodeAt(0));
const ok = Promise.resolve({ tag: "ok" as const, val: undefined });
/** Requests this revision's instance has served; a reload starts a fresh count. */
let served = 0;

/** Fields from text or raw byte values, duplicates kept. */
function fields(entries: [string, string | Uint8Array][]): Fields {
  return Fields.fromList(
    entries.map(([name, value]) => [name, typeof value === "string" ? text(value) : value]),
  );
}

function create(self: Declaration) {
  return { ledger: self.get<typeof Ledger>("example:ingress/ledger@1.0.0", ["main"]) };
}
type Environment = ReturnType<typeof create>;

/** A trailers promise settled when the body producer finishes. */
function later() {
  let settle!: (fields: Fields) => void;
  const trailers = new Promise<{ tag: "ok"; val: Fields }>((resolve) => {
    settle = (fields) => resolve({ tag: "ok", val: fields });
  });
  return { trailers, settle };
}

/** Record how the transmission ended, through the dependency, after the delivery. */
function observe(
  environment: Environment,
  name: string,
  transmitted: PromiseLike<{ tag: string }>,
) {
  void transmitted.then((result) => environment.ledger.note(`${name}-transmitted:${result.tag}`));
}

export default orb({
  name: "ingress",
  apiVersion: 1,
  // The fixed WIT dependency belongs to this revision and is shared by its request handlers.
  create,
  services: {
    handler: {
      create: (self: Declaration) => self.orb as Environment,
      methods: {
        async handle(environment: Environment, _context: Context, request: Request) {
          const path = request.getPathWithQuery() ?? "/";
          served++;
          if (path === "/revision")
            return Response.new(fields([]), writeChunks([text(String(served))]), ok)[0];
          if (path === "/sse") {
            // Head and first event before the request body ends; ticks as request chunks arrive;
            // a context-checked dependency call after the head; trailers after the body.
            const [body] = Request.consumeBody(request, ok);
            const { trailers, settle } = later();
            async function* events() {
              yield text("data: 1\n\n");
              let ticks = 0;
              for await (const chunk of readChunks(body)) {
                ticks++;
                yield text(`data: tick-${ticks}:${chunk.length}\n\n`);
              }
              const noted = await environment.ledger.note(`sse-after-head:${ticks}`);
              yield text(`data: noted-${noted}\n\n`);
              settle(fields([["x-ticks", String(ticks)]]));
            }
            const [response, transmitted] = Response.new(
              fields([["content-type", "text/event-stream"]]),
              writeChunks(events()),
              trailers,
            );
            observe(environment, "sse", transmitted);
            return response;
          }
          if (path === "/echo") {
            // Raw bytes, duplicate fields and trailers, both ways.
            const method = request.getMethod();
            const headers = request.getHeaders();
            const seen = headers.get("x-raw").length;
            const [body, requestTrailers] = Request.consumeBody(request, ok);
            const [response, transmitted] = Response.new(
              fields([
                ["content-type", "application/octet-stream"],
                ["x-method", method.tag === "other" ? method.val : method.tag],
                ["x-seen", String(seen)],
                ["x-raw", Uint8Array.of(0x80, 0xff)],
                ["x-raw", Uint8Array.of(0x21)],
              ]),
              writeChunks(readChunks(body)),
              requestTrailers,
            );
            response.setStatusCode(201);
            observe(environment, "echo", transmitted);
            return response;
          }
          if (path === "/error")
            throw Object.assign(new Error("denied"), { payload: { tag: "HTTP-request-denied" } });
          if (path.startsWith("/refuse/")) {
            // Typed refusals with payloads: escaped/unicode text, a record, an exact u64.
            const payloads: Record<string, unknown> = {
              escaped: { tag: "internal-error", val: 'quote " slash \\ line\nend é🌍' },
              record: {
                tag: "HTTP-request-header-size",
                val: { fieldName: "x-big", fieldSize: 4294967295 },
              },
              u64: { tag: "HTTP-response-body-size", val: 18446744073709551615n },
              dns: { tag: "DNS-error", val: { rcode: "SERVFAIL", infoCode: 65535 } },
            };
            throw Object.assign(new Error("refused"), {
              payload: payloads[path.slice("/refuse/".length)],
            });
          }
          if (path === "/trap") throw new Error("handler trap");
          if (path === "/idle") {
            // Idle in a request read after the head: only the host's disconnect signals end it.
            const [body] = Request.consumeBody(request, ok);
            async function* events() {
              yield text("data: 1\n\n");
              for (const chunks = readChunks(body); !(await chunks.next()).done;);
              await environment.ledger.note("idle-read-ended");
              yield text("data: late\n\n");
            }
            const [response, transmitted] = Response.new(fields([]), writeChunks(events()), ok);
            observe(environment, "idle", transmitted);
            return response;
          }
          if (path === "/write") {
            // Keep writing until the client goes away.
            async function* events() {
              for (let n = 0; ; n++) {
                yield text(`data: ${n}\n\n`);
                await tembra.wait(5);
              }
            }
            const [response, transmitted] = Response.new(fields([]), writeChunks(events()), ok);
            observe(environment, "write", transmitted);
            return response;
          }
          if (path === "/slow") {
            async function* events() {
              yield text("data: 1\n\n");
              for (;;) await tembra.wait(1000);
            }
            return Response.new(fields([]), writeChunks(events()), ok)[0];
          }
          if (path.startsWith("/status/")) {
            const [response] = Response.new(fields([]), undefined, ok);
            response.setStatusCode(Number(path.slice("/status/".length)));
            return response;
          }
          if (path === "/cookie") {
            return Response.new(fields([["set-cookie", "a=1"]]), writeChunks([text("x")]), ok)[0];
          }
          if (path === "/trailers") {
            return Response.new(
              fields([]),
              writeChunks([text("body")]),
              Promise.resolve({ tag: "ok" as const, val: fields([["x-late", "1"]]) }),
            )[0];
          }
          if (path === "/outbound") {
            // A handler-only Orb has no outgoing HTTP module at all.
            const modules = (globalThis as { __tembraStandard?: Record<string, unknown> })
              .__tembraStandard;
            const client = modules?.["wasi:http/client@0.3.0"] === undefined ? "absent" : "present";
            return Response.new(fields([]), writeChunks([text(client)]), ok)[0];
          }
          const [response] = Response.new(fields([]), undefined, ok);
          response.setStatusCode(404);
          return response;
        },
      },
    },
  },
});
