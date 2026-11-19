/** Generated browser clients call actual native and compiled providers through ordinary RPC. */
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { client } from "../sdk/src/tooling/client.ts";
import { checkPage } from "./chrome.ts";

const binary = process.env.TEMBRA_RPC_HOST;
if (!binary || !process.env.TEMBRA_RPC_COMPONENT || !process.env.TEMBRA_RPC_SHAPES_COMPONENT)
  throw new Error("Use just test-rpc-browser to build and select the real Rust providers");
const directory = await mkdtemp(
  path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-rpc-browser-"),
);
const processes: ReturnType<typeof Bun.spawn<"pipe", "pipe", "pipe">>[] = [];
const settled: Promise<void>[] = [];
const source = `import {get as getCodec} from './example-rpc-resources-codec.ts';
import {RemoteHost} from '@syntropika/tembra-sdk/rpc';
function assert(condition, message) { if (!condition) throw new Error(message); }
async function verify(mode) {
  const socket = new WebSocket(location.origin.replace('http', 'ws') + '/rpc?mode=' + mode);
  socket.binaryType = 'arraybuffer';
  await new Promise((resolve, reject) => { socket.onopen = resolve; socket.onerror = reject; });
  let holdStats = false;
  const heldFrames = [];
  let deliver;
  const connection = new RemoteHost({
    send(frame) { socket.send(frame); },
    listen(receive, closed) {
      deliver = receive;
      const message = event => {
        const bytes = new Uint8Array(event.data);
        const frame = JSON.parse(new TextDecoder().decode(bytes));
        if (holdStats && frame.ok && typeof frame.ok.started === 'number') heldFrames.push(bytes);
        else receive(bytes);
      };
      const ended = () => closed();
      socket.addEventListener('message', message); socket.addEventListener('close', ended);
      return () => { socket.removeEventListener('message', message); socket.removeEventListener('close', ended); };
    },
    close() { socket.close(); }
  }, {maxPending: 2});
  try {
    const codec = await getCodec(connection, {orb: 'codec', key: 'main'});
    const control = await connection.get('control', 'main');
    const decoder = await codec.newDecoder(9007199254740993n);
    assert(await decoder.push(2n) === 9007199254740995n, mode + ' exact synchronous call');
    assert(await codec.consume(await codec.open(7n)) === 7n, mode + ' ownership transfer');
    const [first, list] = await codec.pair(3n);
    assert(await codec.consumeMany([[first, null], {tag:'ok', val:list[1]}]) === 7n, mode + ' resource containers');
    const released = await codec.open(8n);
    const failed = await codec.open(9n);
    let finished = false;
    const pending = decoder.next().then(value => { finished = true; return value; });
    for (let tries=0;;tries++) {
      if ((await control.stats()).started === 1) break;
      if (tries > 1000) throw new Error('provider never suspended');
      await new Promise(resolve => setTimeout(resolve, 2));
    }
    holdStats = true;
    const ordinary = control.stats();
    for (let tries=0; heldFrames.length === 0; tries++) {
      if (tries > 1000) throw new Error('stats response never arrived');
      await new Promise(resolve => setTimeout(resolve, 2));
    }
    released.release();
    let exhausted = false;
    try { await codec.consume(failed); } catch(error) { exhausted = error.code === 'exhausted'; }
    assert(exhausted, mode + ' failed owned submission');
    assert(!connection.channel.closed && !finished, mode + ' release preserves unrelated saturated work');
    holdStats = false;
    for (const frame of heldFrames.splice(0)) deliver(frame);
    await ordinary;
    // A response proves that both earlier release frames reached the host before another release.
    await control.stats();
    decoder.release();
    await control.stats();
    codec.reference.release();
    await control.open();
    assert(await pending === 9007199254740996n, mode + ' release during physical method');
    const current = await getCodec(connection, {orb:'codec',key:'other'});
    const stale = await current.open(2n);
    await control.reload();
    let gone = false;
    try { await stale.total(); } catch(error) { gone = error.code === 'gone' && error.reason === 'resource'; }
    assert(gone, mode + ' stale revision'); stale.release();
    const held = await current.open(5n);
    connection.close();
    let closed = false;
    try { await held.total(); } catch(error) { closed = error.code === 'gone' && error.reason === 'connection'; }
    assert(closed, mode + ' connection ownership'); held.release();
  } finally { connection.close(); }
}
try {
  for (const mode of ['native','wasm']) await verify(mode);
  document.body.textContent='TEMBRA_RPC_PASS';
} catch(error) { document.body.textContent='TEMBRA_RPC_FAIL ' + (error.stack ?? error); }
`;

interface Peer {
  child?: ReturnType<typeof Bun.spawn<"pipe", "pipe", "pipe">>;
  mode: string;
}
let server: ReturnType<typeof Bun.serve<Peer>> | undefined;
try {
  const wit = new URL(
    "../../../crates/orb-macro/native-fixture/wit/deps/rpc-resources/protocol.wit",
    import.meta.url,
  ).pathname;
  await client({ wit, world: "provider", out: directory });
  await writeFile(
    path.join(directory, "package.json"),
    JSON.stringify({ name: "tembra-rpc-browser-check", type: "module" }),
  );
  await mkdir(path.join(directory, "node_modules/@syntropika"), { recursive: true });
  await symlink(
    new URL("../sdk", import.meta.url).pathname,
    path.join(directory, "node_modules/@syntropika/tembra-sdk"),
    "dir",
  );
  const entry = path.join(directory, "browser.ts");
  await writeFile(entry, source);
  const bundle = await Bun.build({
    entrypoints: [entry],
    target: "browser",
    format: "esm",
    conditions: ["source"],
  });
  if (!bundle.success) throw new AggregateError(bundle.logs, "Browser RPC client build failed");
  const javascript = await bundle.outputs[0]!.text();
  server = Bun.serve<Peer>({
    hostname: "127.0.0.1",
    port: 0,
    fetch(request, server) {
      const url = new URL(request.url);
      if (url.pathname === "/rpc") {
        const mode = url.searchParams.get("mode");
        if (mode !== "native" && mode !== "wasm")
          return new Response("Invalid provider", { status: 400 });
        if (server.upgrade(request, { data: { mode } })) return undefined;
        return new Response("WebSocket required", { status: 400 });
      }
      if (url.pathname === "/client.js")
        return new Response(javascript, { headers: { "content-type": "text/javascript" } });
      return new Response(
        '<!doctype html><body>running<script type="module" src="/client.js"></script>',
        { headers: { "content-type": "text/html" } },
      );
    },
    websocket: {
      open(socket) {
        const child = Bun.spawn([binary, socket.data.mode], {
          stdin: "pipe",
          stdout: "pipe",
          stderr: "pipe",
        });
        socket.data.child = child;
        processes.push(child);
        const stderr = new Response(child.stderr).text();
        const task = (async () => {
          const reader = child.stdout.getReader();
          let pending = new Uint8Array();
          try {
            for (;;) {
              const chunk = await reader.read();
              if (chunk.done) break;
              const bytes = new Uint8Array(pending.length + chunk.value.length);
              bytes.set(pending);
              bytes.set(chunk.value, pending.length);
              let start = 0;
              for (let index = 0; index < bytes.length; index++) {
                if (bytes[index] !== 10) continue;
                if (socket.readyState === 1) socket.send(bytes.slice(start, index));
                start = index + 1;
              }
              pending = bytes.slice(start);
            }
          } finally {
            reader.releaseLock();
            socket.close();
          }
          const status = await child.exited;
          const result = JSON.parse(await stderr) as {
            references: number;
            calls: number;
            roots: number;
          };
          if (status !== 0 || result.references || result.calls || result.roots)
            throw new Error("Rust host did not release browser RPC ownership");
        })();
        settled.push(task);
        void task.catch(() => socket.close());
      },
      message(socket, bytes) {
        const child = socket.data.child!;
        if (typeof bytes === "string") {
          socket.close();
          return;
        }
        child.stdin.write(bytes);
        child.stdin.write("\n");
        void Promise.resolve(child.stdin.flush()).catch(() => socket.close());
      },
      close(socket) {
        socket.data.child?.stdin.end();
      },
    },
  });
  await checkPage(server.url.href, "TEMBRA_RPC");
  await Promise.all(settled);
  console.log(
    "Generated browser RPC clients passed against native and compiled Rust-hosted providers",
  );
} finally {
  server?.stop(true);
  for (const child of processes) {
    if (child.exitCode === null) {
      child.stdin.end();
      child.kill();
      await child.exited;
    }
  }
  await Promise.allSettled(settled);
  await rm(directory, { recursive: true, force: true });
}
