import { expect, test } from "bun:test";
import { createRequire } from "node:module";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createServer, type Server, type Socket } from "node:net";
import { createHost } from "../src/node.ts";

// Requires an interpreter built with the incremental transport guest runtime.
const engine = process.env.TEMBRA_WASM_ENGINE;
const binding = createRequire(import.meta.url)("@syntropika/tembra-engine-node");
const wit =
  "package test:transport; interface net { fetch: async func(port: u16) -> string; hold: async func(port: u16) -> bool; } world app { import wasi:sockets/types@0.3.0; export net; }";
const source = `const ascii = (text) => Uint8Array.from(text, (character) => character.charCodeAt(0));
globalThis.__tembraOrb = { services: { net: { methods: {
  async fetch(self, context, port) {
    const stream = await tembra.transport.connect('127.0.0.1', port);
    await stream.write(ascii('PING\\n'));
    const chunks = [];
    for (let chunk = await stream.read(); chunk !== null; chunk = await stream.read())
      chunks.push(String.fromCharCode(...chunk));
    await stream.close();
    return chunks.join('|');
  },
  async hold(self, context, port) {
    const stream = await tembra.transport.connect('127.0.0.1', port);
    try { await stream.read(); return true; }
    finally { await stream.close(); console.info('stream-cleanup-finished'); }
  }
} } } };`;

/** A local peer that answers the first connection in two writes and records later closes. */
async function peer(): Promise<{ server: Server; port: number; closed: Promise<number>[] }> {
  const closes: ((at: number) => void)[] = [];
  const closed = Array.from({ length: 2 }, () => new Promise<number>((r) => closes.push(r)));
  let index = 0;
  const server = createServer((socket: Socket) => {
    const current = index++;
    socket.on("close", () => closes[current]?.(performance.now()));
    socket.on("error", () => {});
    if (current === 0)
      socket.once("data", () => {
        socket.write("PO");
        setTimeout(() => socket.end("NG"), 50);
      });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { server, port: (server.address() as { port: number }).port, closed };
}

test.skipIf(!engine)(
  "source Orbs stream bytes through the Node host and cancellation closes their streams",
  async () => {
    const { server, port, closed } = await peer();
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "wit-"),
    );
    await writeFile(path.join(directory, "transport.wit"), wit);
    const contract = new binding.NodeContract(binding.compileContract(directory, "app"), "app");
    const artifact = new Uint8Array(contract.sourceArtifact(source, "raw", 1));
    await rm(directory, { recursive: true, force: true });
    const logs: string[] = [];
    const host = createHost({
      isolated: {
        engine: new Uint8Array(await readFile(engine!)),
        providers: { network: ["tcp"] },
        onLog: (record) => logs.push(record.message),
      },
    });
    try {
      await host.install({
        artifact,
        grants: { network: [{ provider: "tcp", connect: [`127.0.0.1:${port}`] }] },
      });
      const reference = (await host.get("raw::test:transport/net", "main")) as unknown as {
        fetch(port: number): Promise<string>;
        hold(port: number, context?: unknown): Promise<boolean>;
        release(): void;
      };
      expect(await reference.fetch(port)).toBe("PO|NG");
      // Cancelling the caller closes the callback's stream before its read deadline.
      const started = performance.now();
      const pending = host.root(async (context) => {
        setTimeout(() => context.cancel(), 100);
        return reference.hold(port, context);
      });
      await expect(pending).rejects.toThrow();
      expect((await closed[1]!) - started).toBeLessThan(2000);
      reference.release();
    } finally {
      await host.close();
      server.close();
    }
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(logs).toContain("stream-cleanup-finished");
  },
  30000,
);
