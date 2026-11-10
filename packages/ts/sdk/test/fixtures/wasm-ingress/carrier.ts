// Task-owned native qualification fixture: a handler-only source Orb (no imports), so a native
// port test can drive it through the typed platform carrier without a platform kernel.
import { orb, readChunks, writeChunks } from "@syntropika/tembra-sdk";
import type { Context } from "@syntropika/tembra-sdk";
import { Fields, Request, Response } from "wasi:http/types@0.3.0";

const text = (value: string) => Uint8Array.from(value, (unit) => unit.charCodeAt(0));
const ok = Promise.resolve({ tag: "ok" as const, val: undefined });

export default orb({
  name: "carrier",
  apiVersion: 1,
  services: {
    handler: {
      methods: {
        async handle(_environment: unknown, _context: Context, request: Request) {
          const path = request.getPathWithQuery() ?? "/";
          if (path === "/error")
            throw Object.assign(new Error("denied"), { payload: { tag: "HTTP-request-denied" } });
          if (path.startsWith("/refuse/")) {
            // The compiled handler's typed refusals, so both kinds encode the same WIT values.
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
          const [body] = Request.consumeBody(request, ok);
          let settle!: (fields: Fields) => void;
          const trailers = new Promise<{ tag: "ok"; val: Fields }>((resolve) => {
            settle = (fields) => resolve({ tag: "ok", val: fields });
          });
          async function* events() {
            yield text("data: open\n\n");
            let chunks = 0;
            for await (const chunk of readChunks(body)) {
              chunks++;
              yield chunk;
            }
            yield text("data: end\n\n");
            settle(Fields.fromList([["x-chunks", text(String(chunks))]]));
          }
          return Response.new(
            Fields.fromList([["content-type", text("text/event-stream")]]),
            writeChunks(events()),
            trailers,
          )[0];
        },
      },
    },
  },
});
