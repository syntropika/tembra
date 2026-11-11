import { orb, readChunks, writeChunks } from "@syntropika/tembra-sdk";
import { resolveAddresses } from "wasi:sockets/ip-name-lookup@0.3.0";
import { TcpSocket, UdpSocket, type IpSocketAddress } from "wasi:sockets/types@0.3.0";

const loopback = (port: number): IpSocketAddress => ({
  tag: "ipv4",
  val: { port, address: [127, 0, 0, 1] },
});

function text(address: IpSocketAddress): string {
  return address.tag === "ipv4"
    ? `${address.val.address.join(".")}:${address.val.port}`
    : `[${address.val.address.map((part) => part.toString(16)).join(":")}]:${address.val.port}`;
}

/** The standard error code a refused call carries; anything else is a real failure. */
function code(error: unknown): string {
  const tag = (error as { payload?: { tag?: unknown } }).payload?.tag;
  if (typeof tag !== "string") throw error;
  return tag;
}

async function outcome(operation: () => unknown): Promise<string> {
  try {
    await operation();
    return "ok";
  } catch (error) {
    return code(error);
  }
}

const CHUNK = 64 << 10;
/** Each 64 KiB block has one value, so the guest fills natively; the provider checks every byte. */
const byte = (position: number, seed: number) => (Math.floor(position / CHUNK) + seed) % 251;

/** `size` patterned bytes in 64 KiB chunks. */
function* pattern(size: number, seed: number): Generator<Uint8Array> {
  for (let offset = 0; offset < size; offset += CHUNK)
    yield new Uint8Array(Math.min(CHUNK, size - offset)).fill(byte(offset, seed));
}

/** Read a byte stream to its end, checking each chunk's edges against the pattern. */
async function drain(stream: AsyncIterable<Uint8Array>, seed: number): Promise<number> {
  let total = 0;
  for await (const chunk of readChunks(stream)) {
    const last = total + chunk.length - 1;
    if (chunk[0] !== byte(total, seed) || chunk[chunk.length - 1] !== byte(last, seed))
      throw new Error(`bytes ${total}..${last} differ`);
    total += chunk.length;
  }
  return total;
}

export default orb({
  name: "sockets",
  apiVersion: 1,
  services: {
    probes: {
      methods: {
        async exchange(_environment: unknown, _context: unknown, port: number, size: number) {
          using server = TcpSocket.create("ipv4");
          server.bind(loopback(port));
          const accepts = server.listen()[Symbol.asyncIterator]();
          const listening = server.getIsListening();
          const client = TcpSocket.create("ipv4");
          await client.connect(server.getLocalAddress());
          const first = await accepts.next();
          // Dropping the listen stream does not affect the socket it already delivered.
          await accepts.return?.(undefined);
          if (first.done) throw new Error("listen ended before a connection arrived");
          using accepted = first.value;
          const peers = text(accepted.getRemoteAddress()) === text(client.getLocalAddress());
          const sent = client.send(writeChunks(pattern(size, 1)));
          const [download, downloaded] = client.receive();
          // The send and receive streams keep the connection alive without the handle.
          client[Symbol.dispose]();
          const [upload, uploaded] = accepted.receive();
          // End of stream arrives only after the client's half-close.
          const up = await drain(upload, 1);
          // Lifted WIT futures are handles: `read()` returns their value once.
          const sentResult = await sent.read();
          const uploadResult = await uploaded.read();
          const echoed = accepted.send(writeChunks(pattern(size, 2)));
          const down = await drain(download, 2);
          const echoResult = await echoed.read();
          const downloadResult = await downloaded.read();
          return [
            listening,
            peers,
            up,
            down,
            sentResult.tag,
            uploadResult.tag,
            echoResult.tag,
            downloadResult.tag,
          ].join("|");
        },
        async abandon(_environment: unknown, _context: unknown, port: number) {
          // The pending call borrows the socket, so its handle stays until the Orb retires.
          const socket = TcpSocket.create("ipv4");
          const connecting = socket.connect(loopback(port)).then(
            () => "connected",
            (error: unknown) => code(error),
          );
          return await Promise.race([connecting, tembra.wait(30).then(() => "pending")]);
        },
        async idle(_environment: unknown, _context: unknown, port: number) {
          using socket = TcpSocket.create("ipv4");
          await socket.connect(loopback(port));
          const [stream, done] = socket.receive();
          // A readable end with a pending read must not be dropped: cancel the read, await it,
          // then drop the stream (the runtime refuses "cancel and await before dropping").
          const readable = stream as unknown as {
            read(count: number): Promise<Uint8Array>;
            cancelRead(): unknown;
          } & Disposable;
          const reading = readable.read(64).then(
            (bytes) => (bytes.length ? "data" : "cancelled"),
            () => "cancelled",
          );
          await tembra.wait(20);
          readable.cancelRead();
          const read = await reading;
          readable[Symbol.dispose]();
          return `${read}|${(await done.read()).tag}`;
        },
        async datagrams(
          _environment: unknown,
          _context: unknown,
          port: number,
          peer: number,
          denied: number,
        ) {
          using socket = UdpSocket.create("ipv4");
          socket.bind(loopback(port));
          const local = text(socket.getLocalAddress());
          await socket.send(Uint8Array.of(1, 2, 3), loopback(peer));
          const [first, from] = await socket.receive();
          socket.connect(loopback(peer));
          const remote = text(socket.getRemoteAddress());
          await socket.send(Uint8Array.of(4, 5), undefined);
          const [second] = await socket.receive();
          socket.disconnect();
          const refused = await outcome(() => socket.send(Uint8Array.of(9), loopback(denied)));
          const large = await outcome(() => socket.send(new Uint8Array(65_536), loopback(peer)));
          return [
            local,
            first.join(","),
            text(from),
            remote,
            second.join(","),
            refused,
            large,
          ].join("|");
        },
        async lookup(_environment: unknown, _context: unknown, names: string[]) {
          const results: string[] = [];
          for (const name of names) {
            try {
              const addresses = await resolveAddresses(name);
              results.push(
                addresses
                  .map((address) =>
                    address.tag === "ipv4"
                      ? address.val.join(".")
                      : address.val.map((part) => part.toString(16)).join(":"),
                  )
                  .join(","),
              );
            } catch (error) {
              results.push(code(error));
            }
          }
          return results.join(";");
        },
        async refusals(_environment: unknown, _context: unknown, port: number) {
          const tcp = () => TcpSocket.create("ipv4");
          return [
            `connect:${await outcome(async () => {
              using socket = tcp();
              await socket.connect(loopback(port));
            })}`,
            `bind:${await outcome(() => {
              using socket = tcp();
              socket.bind(loopback(port));
            })}`,
            `implicit:${await outcome(() => {
              using socket = tcp();
              socket.bind({ tag: "ipv4", val: { port: 0, address: [0, 0, 0, 0] } });
            })}`,
            `udp:${await outcome(() => {
              using socket = UdpSocket.create("ipv4");
              socket.bind(loopback(port));
            })}`,
            `lookup:${await outcome(() => resolveAddresses("service.example"))}`,
            `state:${await outcome(() => {
              using socket = tcp();
              socket.getRemoteAddress();
            })}`,
            `family:${await outcome(() => {
              using socket = tcp();
              socket.bind({
                tag: "ipv6",
                val: { port, flowInfo: 0, address: [0, 0, 0, 0, 0, 0, 0, 1], scopeId: 0 },
              });
            })}`,
          ].join(" ");
        },
      },
    },
  },
});
