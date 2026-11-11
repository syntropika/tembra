// The public HTTP helper inside a real source component on the Node host. Providers own HTTP and
// TLS; this covers the helper's request construction, refusals and resource ownership on the
// canonical WIT boundary. Streaming, trailers, retained and dropped responses and throwing uploads
// run in http-source.node.test.mjs with the shared wasm-http fixture.
import { afterAll, beforeAll, expect, test } from "bun:test";
import { createServer, type Server } from "node:http";
import { once } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createHost } from "../src/node.ts";
import { build } from "../src/tooling/index.ts";

const sdk = path.resolve(import.meta.dir, "..");
const WIT = `package test:helper;
interface probe {
  denied: async func(url: string) -> string;
  invalid: async func(url: string) -> string;
  captured: async func(url: string) -> string;
  ownership: func() -> string;
}
world app { import wasi:http/client@0.3.0; export probe; }`;
const SOURCE = `import { orb } from ${JSON.stringify(path.join(sdk, "src/guest.ts"))};
import { fetch, HttpError } from ${JSON.stringify(path.join(sdk, "src/http.ts"))};
import { Fields, Request } from "wasi:http/types@0.3.0";

/** The error class and WIT error code an operation was refused with. */
async function refusal(operation: () => Promise<unknown>): Promise<string> {
  try {
    await operation();
    return "accepted";
  } catch (error) {
    const code = error instanceof HttpError ? String((error.code as { tag?: string })?.tag ?? error.code) : "";
    return \`\${(error as Error).name}:\${code}\`;
  }
}

export default orb({ name: "helper", apiVersion: 1, services: { probe: { methods: {
  async denied(_e: unknown, _c: unknown, url: string) {
    return refusal(() => fetch(url));
  },
  async invalid(_e: unknown, _c: unknown, url: string) {
    return refusal(() => fetch(url, { headers: { "bad header": "x" } }));
  },
  async captured(_e: unknown, _c: unknown, url: string) {
    const body = Uint8Array.of(97, 98, 99);
    const pending = fetch(url, { method: "POST", body });
    body.fill(122);
    const response = await pending;
    return response.text();
  },
  ownership() {
    const fields = new Fields();
    fields.append("x-raw", Uint8Array.of(255));
    let refused = "accepted";
    try {
      Request.new(fields, {} as never, Promise.resolve({ tag: "ok", val: undefined }), undefined);
    } catch (error) {
      refused = (error as Error).name;
    }
    // The owned headers precede the bad body: refused argument lowering must not move them.
    const kept = fields.get("x-raw").map((bytes) => bytes[0]).join(",");
    fields[Symbol.dispose]();
    return \`\${refused}:\${kept}\`;
  },
} } } });
`;

let server: Server;
let origin = "";
let artifact: Uint8Array;
const received: { method: string; body: string }[] = [];

beforeAll(async () => {
  server = createServer((request, response) => {
    let body = "";
    request.on("data", (chunk) => (body += chunk));
    request.on("end", () => {
      received.push({ method: request.method!, body });
      response.end(body || "ok");
    });
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  origin = `http://127.0.0.1:${(server.address() as { port: number }).port}`;
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-http-helper-"),
  );
  try {
    await writeFile(path.join(directory, "contract.wit"), WIT);
    await writeFile(path.join(directory, "orb.ts"), SOURCE);
    const [built] = await build({
      root: directory,
      outDir: path.join(directory, "dist"),
      builds: { helper: { entry: "orb.ts", wit: "contract.wit", world: "app", minify: false } },
    });
    artifact = new Uint8Array(await readFile(built!.path));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}, 120_000);

afterAll(() => {
  server?.close();
});

/** Run one probe on a fresh host whose HTTP grant names only `granted`. */
async function probe<T>(granted: string, run: (probe: any) => Promise<T>): Promise<T> {
  const host = createHost({ isolated: { providers: { network: ["outbound"] } } });
  try {
    await host.install({
      artifact,
      grants: { http: [{ provider: "outbound", origins: [granted] }] },
    });
    const reference: any = await host.get("helper::test:helper/probe", "main");
    try {
      return await run(reference);
    } finally {
      reference.release();
    }
  } finally {
    await host.close();
  }
}

test("grant denial and invalid requests are typed refusals that never reach the server", async () => {
  received.length = 0;
  const [denied, invalid] = await probe("http://127.0.0.1:1", async (helper) => [
    await helper.denied(`${origin}/denied`),
    await helper.invalid(`${origin}/invalid`),
  ]);
  expect(denied).toMatch(/^HttpError:/);
  expect(invalid).not.toMatch(/^TypeError/);
  expect(invalid).not.toBe("accepted");
  expect(received).toEqual([]);
}, 60_000);

test("a byte body is captured when fetch is called", async () => {
  received.length = 0;
  expect(await probe(origin, (helper) => helper.captured(`${origin}/captured`))).toBe("abc");
  expect(received).toEqual([{ method: "POST", body: "abc" }]);
}, 60_000);

test("a refused resource argument preserves ownership of earlier owned arguments", async () => {
  expect(await probe(origin, (helper) => helper.ownership())).toBe("TypeError:255");
}, 60_000);
