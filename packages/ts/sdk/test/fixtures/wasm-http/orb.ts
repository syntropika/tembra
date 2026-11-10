import {
  orb,
  type Context,
  type Declaration,
  type Resource,
  type WitMethods,
} from "@syntropika/tembra-sdk";
import { fetch, type HttpResponse } from "@syntropika/tembra-sdk/http";
import type { requests } from "./contract.wit";

interface Pending {
  response: HttpResponse | undefined;
}
function create(self: Declaration) {
  return self.resource<Pending>("response", {
    open: () => ({ response: undefined }),
    close: (pending) => pending.response?.close(),
  });
}
type Environment = Resource<Pending>;

async function inspect(response: HttpResponse) {
  const text = await response.text();
  return {
    status: response.status,
    text,
    duplicateCount: response.headers.getAll("x-repeat").length,
    trailer: (await response.trailers()).get("x-finished"),
  };
}

export default orb({
  name: "http",
  apiVersion: 1,
  services: {
    requests: {
      create,
      methods: {
        async inspect(_environment: Environment, _context: Context, url: string) {
          return inspect(await fetch(url));
        },
        async exchange(_environment: Environment, _context: Context, url: string) {
          async function* chunks() {
            yield Uint8Array.of(1, 2);
            yield Uint8Array.of(3, 4);
          }
          return inspect(
            await fetch(url, {
              method: "POST",
              body: chunks(),
              headers: [
                ["x-raw", Uint8Array.of(255)],
                ["x-raw", Uint8Array.of(254)],
              ],
              trailers: Promise.resolve([["x-upload", "complete"]] as [string, string][]),
            }),
          );
        },
        async duplex(_environment: Environment, _context: Context, url: string) {
          async function* chunks() {
            yield Uint8Array.of(1);
            await tembra.wait(50);
            yield Uint8Array.of(2);
          }
          const response = await fetch(url, { method: "POST", body: chunks() });
          for await (const event of response.events()) return event.data;
          throw new Error("SSE ended without an event");
        },
        async upload(
          _environment: Environment,
          _context: Context,
          url: string,
          count: number,
          fail: boolean,
        ) {
          async function* chunks() {
            for (let index = 0; index < count; index++) {
              yield new Uint8Array(64 << 10).fill(index % 251);
              if (fail && index === 1) throw new Error("upload producer failed");
            }
          }
          const response = await fetch(url, { method: "POST", body: chunks() });
          return response.text();
        },
        async open(environment: Environment, _context: Context, url: string) {
          await environment.value.response?.close();
          environment.value.response = await fetch(url);
          return environment.value.response.status;
        },
        async read(environment: Environment) {
          const response = environment.value.response;
          if (!response) throw new Error("No response is open");
          try {
            return await response.text();
          } finally {
            await response.close();
            environment.value.response = undefined;
          }
        },
        async close(environment: Environment) {
          await environment.value.response?.close();
          environment.value.response = undefined;
        },
      } satisfies WitMethods<typeof requests, Environment>,
    },
  },
});
