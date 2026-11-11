import { orb, readChunks, readText, writeChunks, writeText } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";
import { TcpSocket, type IpSocketAddress } from "wasi:sockets/types@0.3.0";

const loopback = (port: number): IpSocketAddress => ({
  tag: "ipv4",
  val: { port, address: [127, 0, 0, 1] },
});

/** The standard error code a refused call carries; anything else is a real failure. */
function code(error: unknown): string {
  const tag = (error as { payload?: { tag?: unknown } }).payload?.tag;
  if (typeof tag !== "string") throw error;
  return tag;
}

async function outcome(operation: () => Promise<unknown>): Promise<string> {
  try {
    await operation();
    return "ok";
  } catch (error) {
    return code(error);
  }
}

/** The granted preopen containing `path`, with every other preopen released. */
function mount(path: string) {
  const directories = getDirectories();
  const matching = directories.filter(([, name]) => path.startsWith(name + "/"));
  matching.sort((a, b) => b[1].length - a[1].length);
  const selected = matching[0];
  for (const [directory] of directories)
    if (directory !== selected?.[0]) directory[Symbol.dispose]();
  if (!selected) throw new Error("No granted mount matches the path");
  return { directory: selected[0], relative: path.slice(selected[1].length + 1) };
}

const ascii = (text: string) => Uint8Array.from(text, (c) => c.charCodeAt(0));

export default orb({
  name: "files",
  apiVersion: 1,
  services: {
    files: {
      methods: {
        async read() {
          const selected = mount("/data/input");
          using directory = selected.directory;
          using file = await directory.openAt({}, selected.relative, {}, { read: true });
          return readText(file);
        },
        async denied() {
          const selected = mount("/data/input");
          using directory = selected.directory;
          // Await inside the `using` scope: the directory stays borrowed until openAt settles.
          return await outcome(async () => {
            using file = await directory.openAt(
              {},
              selected.relative,
              { truncate: true },
              { write: true },
            );
            await writeText(file, "changed");
          });
        },
        async escape() {
          const selected = mount("/data/input");
          using directory = selected.directory;
          // Await inside the `using` scope: the directory stays borrowed until openAt settles.
          return await outcome(async () => {
            const file = await directory.openAt({}, "../secret", {}, { read: true });
            file[Symbol.dispose]();
          });
        },
        async request(_environment: unknown, _context: unknown, port: number) {
          using socket = TcpSocket.create("ipv4");
          await socket.connect(loopback(port));
          const sent = socket.send(writeChunks([ascii("GET /mobile HTTP/1.0\r\n\r\n")]));
          const [stream, done] = socket.receive();
          let text = "";
          for await (const chunk of readChunks(stream)) text += String.fromCharCode(...chunk);
          const sentResult = await sent.read();
          const doneResult = await done.read();
          if (sentResult.tag !== "ok" || doneResult.tag !== "ok")
            throw new Error(`socket ended with ${sentResult.tag}/${doneResult.tag}`);
          return text;
        },
        async connect(_environment: unknown, _context: unknown, port: number) {
          // Await inside the `using` scope: the directory stays borrowed until openAt settles.
          return await outcome(async () => {
            using socket = TcpSocket.create("ipv4");
            await socket.connect(loopback(port));
          });
        },
        async abandon(_environment: unknown, _context: unknown, port: number) {
          // Abandon a pending read the way the component runtime requires: cancel it, wait for
          // it to settle, then drop the stream and the socket. The peer never sends anything, so
          // only these drops can close the connection.
          using socket = TcpSocket.create("ipv4");
          await socket.connect(loopback(port));
          const [stream, done] = socket.receive();
          const readable = stream as unknown as {
            read(count: number): Promise<Uint8Array>;
            cancelRead(): unknown;
          } & Disposable;
          const reading = readable.read(64).then(
            (chunk) => (chunk.length === 0 ? "cancelled" : "data"),
            () => "rejected",
          );
          readable.cancelRead();
          const read = await reading;
          readable[Symbol.dispose]();
          return `${read}|${(await done.read()).tag}`;
        },
      },
    },
  },
});
