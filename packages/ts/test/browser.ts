import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

const precision = await Bun.build({
  entrypoints: [new URL("../sdk/test/precision.ts", import.meta.url).pathname],
  target: "browser",
  format: "esm",
});
if (!precision.success) throw new AggregateError(precision.logs, "Precision fixture build failed");
const precisionSource = await precision.outputs[0]!.text();
const rootGate = await Bun.build({
  entrypoints: [new URL("../sdk/test/root-gate.ts", import.meta.url).pathname],
  target: "browser",
  format: "esm",
});
if (!rootGate.success) throw new AggregateError(rootGate.logs, "Root gate fixture build failed");
const rootGateSource = await rootGate.outputs[0]!.text();

const contexts = await Bun.build({
  entrypoints: [new URL("../sdk/test/context-values.ts", import.meta.url).pathname],
  target: "browser",
  format: "esm",
});
if (!contexts.success) throw new AggregateError(contexts.logs, "Context fixture build failed");
const contextSource = await contexts.outputs[0]!.text();

const worker = `import {createHost} from '/browser.js';
import {WasmKernel} from '/packages/ts/engine-wasm/dist/tembra.js';
import {verifyIdentifierPrecision} from '/precision.js';
import {verifyExclusiveRootGate} from '/root-gate.js';
import {verifyContextValues} from '/contexts.js';
onmessage = async () => {
  try {
    const host = await createHost();
    if(verifyIdentifierPrecision(()=>new WasmKernel())!==9) throw new Error('Worker identity precision failed');
    await verifyExclusiveRootGate(host);
    await verifyContextValues(host);
    await host.install({name:'worker',apiVersion:1,services:{worker:{create(self){return {state:self.state(0n)}},methods:{add({state},ctx,value){return state.update(ctx,current=>current+value)}}}}});
    const ref = await host.get('worker','main');
    const value = await ref.add(1n << 100n);
    ref.release(); await host.close();
    postMessage({value:value.toString(),objects:host.diagnostics().objects});
  } catch(error) { postMessage({error:String(error.stack ?? error)}); }
};`;
const html = `<!doctype html><html><body>running<script type="module">
import {createHost} from '/browser.js';
import {WasmKernel} from '/packages/ts/engine-wasm/dist/tembra.js';
import {verifyIdentifierPrecision} from '/precision.js';
import {verifyExclusiveRootGate} from '/root-gate.js';
import {verifyContextValues} from '/contexts.js';
try {
  const host = await createHost();
  if(verifyIdentifierPrecision(()=>new WasmKernel())!==9) throw new Error('Browser identity precision failed');
  await verifyExclusiveRootGate(host);
    await verifyContextValues(host);
  await host.install({name:'browser',apiVersion:1,services:{counter:{create(self){return {state:self.state({value:0,items:[]})}},methods:{add({state},ctx){state.update(ctx,current=>({...current,value:current.value+1,items:[...current.items,1]}))}}}}});
  const ref = await host.get('counter','main');
  let updates = 0;
  const detach = ref.state.select(state=>state.items.length).subscribe(()=>updates++);
  const before = ref.state.value;
  for(let index=0;index<1000;index++) if(ref.state.value!==before) throw new Error('Local snapshot identity changed');
  await ref.add();
  if(ref.state.value.value!==1 || before.value!==0 || updates!==2 || !Object.isFrozen(ref.state.value.items)) throw new Error('Browser state contract failed');
  const worker = new Worker('/worker.js',{type:'module'});
  const result = await new Promise((resolve,reject)=>{worker.onmessage=event=>resolve(event.data);worker.onerror=reject;worker.postMessage(null)});
  worker.terminate();
  if(result.error || result.value!==(1n<<100n).toString() || result.objects!==0) throw new Error(JSON.stringify(result));
  detach(); ref.release(); await host.close();
  if(host.diagnostics().values!==0 || host.diagnostics().objects!==0) throw new Error('Browser cleanup failed');
  document.body.textContent='TEMBRA_BROWSER_PASS';
} catch(error) { document.body.textContent='TEMBRA_BROWSER_FAIL '+(error.stack ?? error); }
</script></body></html>`;

const browserSDK = await Bun.build({
  entrypoints: [new URL("../sdk/src/browser.ts", import.meta.url).pathname],
  target: "browser",
  format: "esm",
  plugins: [
    {
      name: "browser-engine-asset",
      setup(builder) {
        builder.onResolve({ filter: /^@syntropika\/tembra-engine-wasm$/ }, () => ({
          path: "/packages/ts/engine-wasm/dist/tembra.js",
          external: true,
        }));
      },
    },
  ],
});
if (!browserSDK.success) throw new AggregateError(browserSDK.logs, "Browser SDK build failed");
const browserSource = await browserSDK.outputs[0]!.text();
const root = path.resolve(import.meta.dir, "../../..");
const server = Bun.serve({
  hostname: "127.0.0.1",
  port: 0,
  async fetch(request) {
    const pathname = new URL(request.url).pathname;
    if (pathname === "/") return new Response(html, { headers: { "content-type": "text/html" } });
    if (pathname === "/root-gate.js")
      return new Response(rootGateSource, { headers: { "content-type": "text/javascript" } });
    if (pathname === "/contexts.js")
      return new Response(contextSource, { headers: { "content-type": "text/javascript" } });
    if (pathname === "/precision.js")
      return new Response(precisionSource, { headers: { "content-type": "text/javascript" } });
    if (pathname === "/browser.js")
      return new Response(browserSource, { headers: { "content-type": "text/javascript" } });
    if (pathname === "/worker.js")
      return new Response(worker, { headers: { "content-type": "text/javascript" } });
    if (!pathname.startsWith("/packages/ts/engine-wasm/dist/"))
      return new Response("Not found", { status: 404 });
    const filePath = path.resolve(root, `.${pathname}`);
    if (!filePath.startsWith(path.join(root, "packages/ts/engine-wasm/dist/")))
      return new Response("Not found", { status: 404 });
    const file = Bun.file(filePath);
    return (await file.exists()) ? new Response(file) : new Response("Not found", { status: 404 });
  },
});
const profile = await mkdtemp(path.join(tmpdir(), "tembra-browser-"));
let browser: ReturnType<typeof Bun.spawn> | undefined;
let socket: WebSocket | undefined;
try {
  browser = Bun.spawn(
    [
      process.env.TEMBRA_BROWSER ?? "google-chrome",
      "--headless=new",
      "--disable-gpu",
      `--user-data-dir=${profile}`,
      "--remote-debugging-port=0",
      "--no-first-run",
      "about:blank",
    ],
    { stdout: "ignore", stderr: "pipe" },
  );
  const reader = (browser.stderr as ReadableStream<Uint8Array>).getReader();
  let diagnostics = "";
  let endpoint: string | undefined;
  while (!endpoint) {
    const chunk = await reader.read();
    if (chunk.done)
      throw new Error(`Browser exited before its debugging endpoint opened: ${diagnostics}`);
    diagnostics += new TextDecoder().decode(chunk.value);
    endpoint = diagnostics.match(/DevTools listening on (ws:\/\/\S+)/)?.[1];
  }
  socket = new WebSocket(endpoint);
  await new Promise<void>((resolve, reject) => {
    socket!.onopen = () => resolve();
    socket!.onerror = reject;
  });
  let next = 0;
  const pending = new Map<number, { resolve(value: any): void; reject(error: unknown): void }>();
  socket.onmessage = (event) => {
    const response = JSON.parse(String(event.data));
    const request = pending.get(response.id);
    if (!request) return;
    pending.delete(response.id);
    if (response.error) request.reject(new Error(JSON.stringify(response.error)));
    else request.resolve(response.result);
  };
  function command(method: string, params: unknown = {}, sessionId?: string): Promise<any> {
    const id = ++next;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject });
      socket!.send(JSON.stringify({ id, method, params, sessionId }));
    });
  }
  const target = await command("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await command("Target.attachToTarget", {
    targetId: target.targetId,
    flatten: true,
  });
  await command("Page.navigate", { url: server.url.href }, sessionId);
  const deadline = Date.now() + 30000;
  let output = "";
  while (Date.now() < deadline) {
    const response = await command(
      "Runtime.evaluate",
      { expression: "document.body?.textContent ?? ''", returnByValue: true },
      sessionId,
    );
    output = response.result.value ?? "";
    if (output === "TEMBRA_BROWSER_PASS") {
      console.log(
        "Browser and Worker acceptance passed, including context values, root gates and 9 identifier precision boundaries each",
      );
      break;
    }
    if (output.startsWith("TEMBRA_BROWSER_FAIL")) throw new Error(output);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  if (output !== "TEMBRA_BROWSER_PASS") throw new Error(`Browser acceptance timed out: ${output}`);
} finally {
  socket?.close();
  browser?.kill();
  if (browser) await browser.exited;
  server.stop(true);
  await rm(profile, { recursive: true, force: true });
}
