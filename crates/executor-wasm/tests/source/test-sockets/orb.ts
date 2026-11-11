import { orb, readChunks, writeChunks } from "@syntropika/tembra-sdk";
import { TcpSocket, type IpSocketAddress } from "wasi:sockets/types@0.3.0";

const loopback = (port: number): IpSocketAddress => ({
  tag: "ipv4",
  val: { port, address: [127, 0, 0, 1] },
});
const ascii = (value: string) => Uint8Array.from(value, (unit) => unit.charCodeAt(0));
const text = (bytes: Uint8Array) => String.fromCharCode(...bytes);
/** The runtime's readable end of a `stream<u8>`, read one bounded chunk at a time. */
type Readable = {
  read(count: number): Promise<Uint8Array>;
  cancelRead(): unknown;
} & Disposable;

async function connect(port: number): Promise<TcpSocket> {
  const socket = TcpSocket.create("ipv4");
  try {
    await socket.connect(loopback(port));
  } catch (error) {
    socket[Symbol.dispose]();
    throw error;
  }
  return socket;
}

export default orb({
  name: "raw",
  apiVersion: 1,
  services: {
    net: {
      methods: {
        // The peer sends the rest of its answer only after ACK, so the first chunk was
        // delivered before the rest existed.
        async fetch(_environment: unknown, _context: unknown, port: number) {
          using socket = await connect(port);
          const [stream, done] = socket.receive();
          let seen!: () => void;
          const first = new Promise<void>((resolve) => {
            seen = resolve;
          });
          const sent = socket.send(
            writeChunks(
              (async function* () {
                yield ascii("PING\n");
                await first;
                yield ascii("ACK\n");
              })(),
            ),
          );
          let head: string | null = null;
          let all = "";
          for await (const chunk of readChunks(stream)) {
            if (head === null) {
              head = text(chunk);
              seen();
            }
            all += text(chunk);
          }
          const results = [(await sent.read()).tag, (await done.read()).tag];
          if (results.some((tag) => tag !== "ok")) throw new Error(`exchange failed: ${results}`);
          return JSON.stringify({ first: head, all });
        },
        // Dropping the socket alone releases only its own handle: the stream still reads.
        async alive(_environment: unknown, _context: unknown, port: number) {
          const socket = await connect(port);
          const [stream, done] = socket.receive();
          socket[Symbol.dispose]();
          let all = "";
          for await (const chunk of readChunks(stream)) all += text(chunk);
          if ((await done.read()).tag !== "ok") throw new Error("receive failed");
          return all;
        },
        // Dropping the last handles closes the connection although the peer never sent anything.
        async abandon(_environment: unknown, _context: unknown, port: number) {
          const socket = await connect(port);
          const [stream, done] = socket.receive();
          const readable = stream as unknown as Readable;
          const reading = readable.read(64).catch(() => new Uint8Array());
          readable.cancelRead();
          await reading;
          readable[Symbol.dispose]();
          done.drop();
          socket[Symbol.dispose]();
          return true;
        },
        // The caller cancels; the pending read settles and the cleanup drops every handle.
        async hold(_environment: unknown, _context: unknown, port: number) {
          const socket = await connect(port);
          const [stream, done] = socket.receive();
          const readable = stream as unknown as Readable;
          try {
            // A revoked read may also end without data; only bytes count as an answer.
            if ((await readable.read(64)).length === 0) throw new Error("ended without data");
            return true;
          } catch (error) {
            throw new Error(`receive failed: ${(error as Error).message}`);
          } finally {
            readable[Symbol.dispose]();
            done.drop();
            socket[Symbol.dispose]();
          }
        },
        async forbidden(_environment: unknown, _context: unknown, port: number) {
          try {
            (await connect(port))[Symbol.dispose]();
            return false;
          } catch {
            return true;
          }
        },
      },
    },
  },
});
