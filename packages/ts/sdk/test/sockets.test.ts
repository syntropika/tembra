import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer, type Socket } from "node:net";
import { createHost } from "../src/node.ts";
import { build } from "../src/tooling/index.ts";

// The fixture exercises authored TypeScript imports through the normal preparation path.
const engine = process.env.TEMBRA_WASM_ENGINE;
const wit =
  "package test:sockets; interface net { fetch: async func(port: u16) -> string; hold: async func(port: u16) -> bool; } world app { import wasi:sockets/types@0.3.0; export net; }";
const source = `import {orb, readChunks, writeChunks} from "@syntropika/tembra-sdk";
import {TcpSocket} from "wasi:sockets/types@0.3.0";
export default orb({name:"raw",apiVersion:1,services:{net:{methods:{
  async fetch(_self: unknown, _context: unknown, port: number) {
    using socket = TcpSocket.create("ipv4");
    await socket.connect({tag:"ipv4",val:{port,address:[127,0,0,1]}});
    const sent = socket.send(writeChunks([new Uint8Array([80,73,78,71,10])]));
    const [stream, received] = socket.receive();
    const chunks: string[] = [];
    for await (const chunk of readChunks(stream)) chunks.push(String.fromCharCode(...chunk));
    if ((await sent).tag !== "ok" || (await received).tag !== "ok") throw new Error("TCP stream failed");
    return chunks.join("");
  },
  async hold(_self: unknown, _context: unknown, port: number) {
    using socket = TcpSocket.create("ipv4");
    await socket.connect({tag:"ipv4",val:{port,address:[127,0,0,1]}});
    const [stream, received] = socket.receive();
    try {
      for await (const _chunk of readChunks(stream)) return true;
      return false;
    } finally {
      try { await received; } catch {}
      console.info("stream-cleanup-finished");
    }
  }
}}}});
`;

/** A local peer that answers the first connection in two writes and records later closes. */
async function peer(): Promise<{
  port: number;
  closed: Promise<number>[];
  close(): Promise<void>;
}> {
  const closes: ((at: number) => void)[] = [];
  const closed = Array.from({ length: 2 }, () => new Promise<number>((r) => closes.push(r)));
  let index = 0;
  const connections = new Set<Socket>();
  const server = createServer({ allowHalfOpen: true }, (socket: Socket) => {
    const current = index++;
    connections.add(socket);
    socket.on("close", () => connections.delete(socket));
    if (current !== 0) socket.on("end", () => socket.end());
    socket.on("close", () => closes[current]?.(performance.now()));
    socket.on("error", () => {});
    if (current === 0)
      socket.once("data", () => {
        socket.write("PO");
        setTimeout(() => socket.end("NG"), 50);
      });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    port: (server.address() as { port: number }).port,
    closed,
    async close() {
      for (const socket of connections) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

test.skipIf(!engine)(
  "source Orbs stream bytes through the Node host and cancellation closes their streams",
  async () => {
    const directory = await mkdtemp(
      path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "wit-"),
    );
    let artifact: Uint8Array;
    try {
      await writeFile(
        path.join(directory, "package.json"),
        '{"name":"socket-consumer","type":"module"}',
      );
      // Resolve the SDK as an installed dependency, independently of ancestor checkouts.
      await mkdir(path.join(directory, "node_modules/@syntropika"), { recursive: true });
      await symlink(
        fileURLToPath(new URL("../", import.meta.url)),
        path.join(directory, "node_modules/@syntropika/tembra-sdk"),
        "dir",
      );
      await writeFile(path.join(directory, "contract.wit"), wit);
      await writeFile(path.join(directory, "orb.ts"), source);
      const [built] = await build({
        root: directory,
        builds: { sockets: { entry: "orb.ts", wit: "contract.wit", world: "app" } },
      });
      artifact = new Uint8Array(await readFile(built!.path));
    } finally {
      await rm(directory, { recursive: true, force: true });
    }
    const { port, closed, close } = await peer();
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
      const reference = (await host.get("raw::test:sockets/net", "main")) as unknown as {
        fetch(port: number): Promise<string>;
        hold(port: number, context?: unknown): Promise<boolean>;
        release(): void;
      };
      expect(await reference.fetch(port)).toBe("PONG");
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
      await close();
    }
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(logs).toContain("stream-cleanup-finished");
  },
  30000,
);
