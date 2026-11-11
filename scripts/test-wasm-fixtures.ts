import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseArgs } from "node:util";
import { build } from "../packages/ts/sdk/src/tooling/index.ts";

// Every compiled component is also prepared for browsers, so Chrome qualifies exactly these bytes.
const compiledInputs = {
  provider: "the compiled arithmetic provider",
  contributor: "the compiled hook contributor",
  consumer: "the compiled pipeline consumer",
  "keyed-router": "the compiled keyed-reference router",
  probe: "the compiled probe provider",
  "http-client": "the compiled wasi:http client",
  sockets: "the compiled socket and DNS probe",
  system: "the compiled clocks, random and CLI probe",
  filesystem: "the compiled descriptor and directory probe",
  "http-handler": "the compiled wasi:http handler",
  "resource-provider": "the compiled resource provider",
  "resource-consumer": "the compiled resource consumer",
  "stream-producer": "the compiled stream producer",
  "stream-consumer": "the compiled stream consumer",
} as const;
const { values } = parseArgs({
  options: {
    out: { type: "string" },
    provider: { type: "string" },
    contributor: { type: "string" },
    consumer: { type: "string" },
    "keyed-router": { type: "string" },
    probe: { type: "string" },
    "http-client": { type: "string" },
    sockets: { type: "string" },
    system: { type: "string" },
    filesystem: { type: "string" },
    "http-handler": { type: "string" },
    "resource-provider": { type: "string" },
    "resource-consumer": { type: "string" },
    "stream-producer": { type: "string" },
    "stream-consumer": { type: "string" },
  },
});
if (!values.out) throw new Error("Supply --out with an external qualification artifact directory");
/** The resolved path of one required compiled component input. */
function component(name: keyof typeof compiledInputs): string {
  const file = values[name];
  if (!file) throw new Error(`Supply --${name} with ${compiledInputs[name]} component`);
  return path.resolve(file);
}
for (const name of Object.keys(compiledInputs)) component(name as keyof typeof compiledInputs);
const root = path.resolve(import.meta.dir, "..");
const directory = path.resolve(values.out);
const sources = path.join(directory, "sources");
await mkdir(path.join(sources, "calculation"), { recursive: true });
await mkdir(path.join(sources, "storage"), { recursive: true });
const guest = JSON.stringify(path.join(root, "packages/ts/sdk/src/guest.ts"));
const http = JSON.stringify(path.join(root, "packages/ts/sdk/src/http.ts"));
const fixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-orb");
let calculator = await readFile(path.join(fixture, "orb.ts"), "utf8");
calculator = calculator
  .replaceAll('"@syntropika/tembra-sdk/http"', http)
  .replaceAll('"@syntropika/tembra-sdk"', guest);
await writeFile(
  path.join(sources, "calculation/contract.wit"),
  await readFile(path.join(fixture, "contract.wit")),
);
const builds: Record<string, import("../packages/ts/sdk/src/tooling/types.ts").BuildTarget> = {};
for (const [name, source, world] of [
  ["calculator", calculator, "calculator"],
  [
    "revised",
    calculator.replace("Expected guest failure", "Expected revised guest failure"),
    "calculator",
  ],
  [
    "online",
    (await readFile(path.join(fixture, "online.ts"), "utf8"))
      .replaceAll('"@syntropika/tembra-sdk/http"', http)
      .replaceAll('"@syntropika/tembra-sdk"', guest),
    "online-calculator",
  ],
] as const) {
  await mkdir(path.join(sources, world), { recursive: true });
  await writeFile(
    path.join(sources, world, "contract.wit"),
    await readFile(path.join(fixture, "contract.wit")),
  );
  await writeFile(path.join(sources, world, `${name}.ts`), source);
  builds[name] = {
    entry: `${world}/${name}.ts`,
    wit: `${world}/contract.wit`,
    world,
    // The reload regression edits its known source text; the HTTP fixture uses production minification.
    minify: name === "online",
  };
}
const guard = `import {orb,type Declaration} from ${guest};
export default orb({name: "NAME",apiVersion:1,services:{arithmetic:{
create(self:Declaration){if(self.key[0]==="broken")throw new Error("factory refused");return {};},
methods:{async double(_e:unknown,_c:unknown,value:number){
if(value===1){while(true){}} if(value===2)await new Promise(()=>{});
if(value===4){tembra.wait(50);return 42;} if(value===5)throw new Error("upload interrupted"); return value*2;
}}}}});`;
for (const name of ["arithmetic", "cpu", "suspended"]) {
  await writeFile(
    path.join(sources, "calculation", `${name}.ts`),
    name === "arithmetic"
      ? `import {orb} from ${guest}; export default orb({name:"arithmetic",apiVersion:1,services:{arithmetic:{methods:{async double(_e:unknown,_c:unknown,value:number){if(value===6){try{await tembra.wait(200)}finally{console.info("slow-finished")}}return value*2;}}}}});`
      : guard.replace('"NAME"', JSON.stringify(name)),
  );
  builds[`${name}-source`] = {
    entry: `calculation/${name}.ts`,
    wit: "calculation/contract.wit",
    world: "arithmetic-provider",
    minify: false,
  };
}
await writeFile(
  path.join(sources, "storage/storage.wit"),
  await readFile(path.join(root, "crates/executor-wasm/tests/wit/storage/storage.wit")),
);
for (const name of ["files", "writer", "reader", "persistent", "persistent-reader"]) {
  await writeFile(
    path.join(sources, "storage", `${name}.ts`),
    `import {orb,readText,writeText} from ${guest};
import {getDirectories} from "wasi:filesystem/preopens@0.3.0";
function mount(path:string) {
 const directories=getDirectories();
 const matching=directories.filter(([,name])=>path.startsWith(name+"/"));
 matching.sort((a,b)=>b[1].length-a[1].length);
 const selected=matching[0];
 for(const [directory] of directories)if(directory!==selected?.[0])directory[Symbol.dispose]();
 if(!selected)throw new Error("No granted mount matches the path");
 return {directory:selected[0],relative:path.slice(selected[1].length+1)};
}
export default orb({name:${JSON.stringify(name)},apiVersion:1,services:{files:{methods:{
async read(_e:unknown,_c:unknown,path:string){
 const selected=mount(path);using directory=selected.directory;
 using file=await directory.openAt({},selected.relative,{}, {read:true});
 return readText(file);
},
async write(_e:unknown,_c:unknown,path:string,value:string){
 const selected=mount(path);using directory=selected.directory;
 using file=await directory.openAt({},selected.relative,{create:true,truncate:true},{write:true});
 await writeText(file,value);
},
}}}});`,
  );
  builds[name] = {
    entry: `storage/${name}.ts`,
    wit: "storage/storage.wit",
    world: "storage",
    minify: false,
  };
}
for (const world of ["provider", "extension"]) {
  const target = `hook-${world}`;
  await mkdir(path.join(sources, target), { recursive: true });
  await writeFile(
    path.join(sources, target, "contract.wit"),
    await readFile(path.join(root, "crates/executor-wasm/tests/wit/hooks/hooks.wit")),
  );
  const source = await readFile(
    path.join(root, `packages/ts/sdk/test/fixtures/wasm-hooks/${world}.ts`),
    "utf8",
  );
  await writeFile(
    path.join(sources, target, "orb.ts"),
    source.replaceAll('"@syntropika/tembra-sdk"', guest),
  );
  builds[target] = { entry: `${target}/orb.ts`, wit: `${target}/contract.wit`, world };
}
// HTTP uses the same source bundle and standard imports in native and browser hosts.
const httpFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-http");
await mkdir(path.join(sources, "http"), { recursive: true });
await writeFile(
  path.join(sources, "http/contract.wit"),
  await readFile(path.join(httpFixture, "contract.wit")),
);
await writeFile(
  path.join(sources, "http/orb.ts"),
  (await readFile(path.join(httpFixture, "orb.ts"), "utf8"))
    .replaceAll('"@syntropika/tembra-sdk/http"', http)
    .replaceAll('"@syntropika/tembra-sdk"', guest),
);
builds.http = { entry: "http/orb.ts", wit: "http/contract.wit", world: "client" };
// Sockets: one authored source under two Orb names, so acceptance can install them with different
// providers and grants.
const socketsFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-sockets");
await mkdir(path.join(sources, "sockets"), { recursive: true });
await writeFile(
  path.join(sources, "sockets/contract.wit"),
  await readFile(path.join(socketsFixture, "contract.wit")),
);
const sockets = (await readFile(path.join(socketsFixture, "orb.ts"), "utf8")).replaceAll(
  '"@syntropika/tembra-sdk"',
  guest,
);
for (const [name, source] of [
  ["sockets", sockets],
  ["sockets-peer", sockets.replace('name: "sockets"', 'name: "sockets-peer"')],
] as const) {
  await writeFile(path.join(sources, "sockets", `${name}.ts`), source);
  builds[name] = { entry: `sockets/${name}.ts`, wit: "sockets/contract.wit", world: "sockets" };
}
// Incoming HTTP: the handler Orb and the dependency it calls after its response head.
const ingressFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-ingress");
for (const [name, file, world] of [
  ["ingress", "orb.ts", "handler"],
  ["ledger", "ledger.ts", "provider"],
  ["ingress-carrier", "carrier.ts", "handler-only"],
] as const) {
  await mkdir(path.join(sources, `ingress-${world}`), { recursive: true });
  await writeFile(
    path.join(sources, `ingress-${world}/contract.wit`),
    await readFile(path.join(ingressFixture, "contract.wit")),
  );
  await writeFile(
    path.join(sources, `ingress-${world}/${name}.ts`),
    (await readFile(path.join(ingressFixture, file), "utf8")).replaceAll(
      '"@syntropika/tembra-sdk"',
      guest,
    ),
  );
  builds[name] = {
    entry: `ingress-${world}/${name}.ts`,
    wit: `ingress-${world}/contract.wit`,
    world,
    minify: false,
  };
}
// Exact u64/s64: a provider and a consumer relaying through its WIT import.
const precisionFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-precision");
for (const [name, file, world] of [
  ["precision", "provider.ts", "provider"],
  ["precision-relay", "consumer.ts", "consumer"],
] as const) {
  await mkdir(path.join(sources, `precision-${world}`), { recursive: true });
  await writeFile(
    path.join(sources, `precision-${world}/contract.wit`),
    await readFile(path.join(precisionFixture, "contract.wit")),
  );
  await writeFile(
    path.join(sources, `precision-${world}/${name}.ts`),
    (await readFile(path.join(precisionFixture, file), "utf8")).replaceAll(
      '"@syntropika/tembra-sdk"',
      guest,
    ),
  );
  builds[name] = {
    entry: `precision-${world}/${name}.ts`,
    wit: `precision-${world}/contract.wit`,
    world,
    minify: false,
  };
}
// Source byte streams use real WIT types and the SDK's chunk helpers in both directions.
const streamFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-stream-source");
for (const [name, file, world] of [
  ["stream-source", "orb.ts", "producer"],
  ["stream-source-consumer", "consumer.ts", "consumer"],
] as const) {
  await mkdir(path.join(sources, name), { recursive: true });
  await writeFile(
    path.join(sources, name, "contract.wit"),
    await readFile(path.join(streamFixture, "contract.wit")),
  );
  await writeFile(
    path.join(sources, name, file),
    (await readFile(path.join(streamFixture, file), "utf8")).replaceAll(
      '"@syntropika/tembra-sdk"',
      guest,
    ),
  );
  builds[name] = { entry: `${name}/${file}`, wit: `${name}/contract.wit`, world };
}

// Context.get: keyed counters and the router selecting them at runtime under the caller's root.
const keyedFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-keyed");
for (const [name, file, world] of [
  ["keyed", "provider.ts", "provider"],
  ["keyed-router", "router.ts", "consumer"],
] as const) {
  await mkdir(path.join(sources, `keyed-${world}`), { recursive: true });
  await writeFile(
    path.join(sources, `keyed-${world}/contract.wit`),
    await readFile(path.join(keyedFixture, "contract.wit")),
  );
  await writeFile(
    path.join(sources, `keyed-${world}/${name}.ts`),
    (await readFile(path.join(keyedFixture, file), "utf8")).replaceAll(
      '"@syntropika/tembra-sdk"',
      guest,
    ),
  );
  builds[name] = {
    entry: `keyed-${world}/${name}.ts`,
    wit: `keyed-${world}/contract.wit`,
    world,
    minify: false,
  };
}
// The compiled router uses the same keyed provider through host-local WIT references.
await mkdir(path.join(sources, "compiled-keyed-router"), { recursive: true });
await writeFile(
  path.join(sources, "compiled-keyed-router/keyed.wit"),
  await readFile(path.join(root, "crates/executor-wasm/tests/wit/keyed/keyed.wit")),
);
builds["compiled-keyed-router"] = {
  entry: component("keyed-router"),
  wit: "compiled-keyed-router/keyed.wit",
  world: "router",
  apiVersion: 1,
  references: { "example:keyed/counter-ref@1.0.0": "example:keyed/counter@1.0.0" },
  browser: true,
};
// Standard WASI source Orbs: the example scenario and the acceptance-only boundary probes. Two
// boundary revisions let acceptance pass one revision's handle number to the other.
const standardFixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-standard");
// Every source build of one WIT file selects the same world, so each world gets its own copy.
for (const directory of ["standard", "boundary"]) {
  await mkdir(path.join(sources, directory), { recursive: true });
  await writeFile(
    path.join(sources, directory, "contract.wit"),
    await readFile(path.join(standardFixture, "contract.wit")),
  );
}
const boundary = (await readFile(path.join(standardFixture, "boundary.ts"), "utf8"))
  .replaceAll('"@syntropika/tembra-sdk"', guest)
  .replaceAll(
    '"../../../src/interpreter/standard.ts"',
    JSON.stringify(path.join(root, "packages/ts/sdk/src/interpreter/standard.ts")),
  );
for (const [name, source, world] of [
  [
    "standard",
    (await readFile(path.join(standardFixture, "orb.ts"), "utf8")).replaceAll(
      '"@syntropika/tembra-sdk"',
      guest,
    ),
    "standard",
  ],
  ["boundary", boundary, "boundary"],
  ["boundary-peer", boundary.replace('name: "boundary"', 'name: "boundary-peer"'), "boundary"],
] as const) {
  await writeFile(path.join(sources, world, `${name}.ts`), source);
  builds[name] = {
    entry: `${world}/${name}.ts`,
    wit: `${world}/contract.wit`,
    world,
    minify: false,
  };
}
// The real Rust contributor joins all three hooks before the source participants.
builds["hook-compiled"] = {
  entry: component("contributor"),
  wit: "hook-extension/contract.wit",
  world: "extension",
  apiVersion: 1,
  contributes: Object.fromEntries(
    ["guard", "audit", "choice"].map((hook) => [`example:hooks/${hook}@1.0.0`, -7]),
  ),
  browser: true,
};
builds.arithmetic = {
  entry: component("provider"),
  wit: "calculation/contract.wit",
  world: "arithmetic-provider",
  apiVersion: 1,
  browser: true,
};
// The compiled pipeline consumer imports `arithmetic` synchronously and `delayed` through P3
// async lowering; two source providers wait before answering, so browser JSPI must suspend it.
const pipelineWit = path.join(root, "crates/executor-wasm/tests/wit/pipeline/pipeline.wit");
for (const [name, world, service] of [
  ["pipeline-arithmetic", "arithmetic-provider", "arithmetic"],
  ["pipeline-delayed", "delayed-provider", "delayed"],
] as const) {
  await mkdir(path.join(sources, name), { recursive: true });
  await writeFile(path.join(sources, name, "contract.wit"), await readFile(pipelineWit));
  await writeFile(
    path.join(sources, name, "orb.ts"),
    `import {orb} from ${guest};
export default orb({name:${JSON.stringify(name)},apiVersion:1,services:{${service}:{methods:{
async double(_e:unknown,_c:unknown,value:number){await tembra.wait(20);return value*2;}}}}});`,
  );
  builds[name] = { entry: `${name}/orb.ts`, wit: `${name}/contract.wit`, world, minify: false };
}
await mkdir(path.join(sources, "pipeline"), { recursive: true });
await writeFile(path.join(sources, "pipeline/contract.wit"), await readFile(pipelineWit));
builds.pipeline = {
  entry: component("consumer"),
  wit: "pipeline/contract.wit",
  world: "consumer",
  apiVersion: 1,
  browser: true,
};
// Compiled controls (fuel, trap, filesystem) and the standard wasi:http client and handler.
for (const [name, directory, file, world] of [
  ["probe", "probe", "probe.wit", "probe-provider"],
  ["http-client", "http", "http.wit", "client"],
  ["http-handler", "handler", "handler.wit", "handler"],
] as const) {
  await mkdir(path.join(sources, name), { recursive: true });
  await writeFile(
    path.join(sources, name, "contract.wit"),
    await readFile(path.join(root, "crates/executor-wasm/tests/wit", directory, file)),
  );
  builds[name] = {
    entry: component(name),
    wit: `${name}/contract.wit`,
    world,
    apiVersion: 1,
    browser: true,
  };
}
await mkdir(path.join(sources, "filesystem"), { recursive: true });
for (const file of ["contract.wit", "orb.ts"]) {
  const source = await readFile(
    path.join(root, "packages/ts/sdk/test/fixtures/wasm-filesystem", file),
    "utf8",
  );
  await writeFile(
    path.join(sources, "filesystem", file),
    source.replaceAll('"@syntropika/tembra-sdk"', guest),
  );
}
builds.filesystem = {
  entry: "filesystem/orb.ts",
  wit: "filesystem/contract.wit",
  world: "filesystem",
};
for (const name of ["system", "ambient"] as const) {
  const directory = path.join(sources, name);
  await mkdir(directory, { recursive: true });
  const fixture = path.join(root, "packages/ts/sdk/test/fixtures/wasm-system");
  await writeFile(
    path.join(directory, "contract.wit"),
    await readFile(path.join(fixture, "contract.wit")),
  );
  const source = await readFile(
    path.join(fixture, name === "system" ? "orb.ts" : "ambient.ts"),
    "utf8",
  );
  await writeFile(
    path.join(directory, "orb.ts"),
    source.replaceAll('"@syntropika/tembra-sdk"', guest),
  );
  builds[name] = { entry: `${name}/orb.ts`, wit: `${name}/contract.wit`, world: name };
}
// Exercise the public WASI bindings through actual compiled guests as well as source Orbs.
for (const name of ["sockets", "system", "filesystem"] as const) {
  const target = `compiled-${name}`;
  await mkdir(path.join(sources, target), { recursive: true });
  await writeFile(
    path.join(sources, target, "contract.wit"),
    await readFile(path.join(root, "crates/executor-wasm/tests/wit", name, `${name}.wit`)),
  );
  builds[target] = {
    entry: component(name),
    wit: `${target}/contract.wit`,
    world: name,
    apiVersion: 1,
    browser: true,
  };
}
// The same compiled resource and stream fixtures exercise the browser capability path.
for (const [name, file, world] of [
  ["resource-provider", "resources/resources.wit", "provider"],
  ["resource-consumer", "resources/resources.wit", "consumer"],
  ["stream-producer", "streams/streams.wit", "producer"],
  ["stream-consumer", "streams/streams.wit", "consumer"],
] as const) {
  await mkdir(path.join(sources, name), { recursive: true });
  await writeFile(
    path.join(sources, name, "contract.wit"),
    await readFile(path.join(root, "crates/executor-wasm/tests/wit", file)),
  );
  builds[name] = {
    entry: component(name),
    wit: `${name}/contract.wit`,
    world,
    apiVersion: 1,
    browser: true,
  };
}
await mkdir(path.join(sources, "resource-pace"), { recursive: true });
await writeFile(
  path.join(sources, "resource-pace/contract.wit"),
  await readFile(path.join(root, "crates/executor-wasm/tests/wit/resources/resources.wit")),
);
await writeFile(
  path.join(sources, "resource-pace/orb.ts"),
  `import {orb} from ${guest};
export default orb({name:"resource-pace",apiVersion:1,services:{pace:{methods:{
async wait(_e:unknown,_c:unknown,value:bigint){await tembra.wait(20);return value*10n;},
note(){console.info("destructor import reached pace");}}}}});`,
);
builds["resource-pace"] = {
  entry: "resource-pace/orb.ts",
  wit: "resource-pace/contract.wit",
  world: "pacer",
};
const artifacts = await build({ root: sources, outDir: directory, builds });
for (const artifact of artifacts) {
  await writeFile(path.join(directory, `${artifact.name}.wasm`), await readFile(artifact.path));
}
// Every compiled target must have produced its application-owned browser adapter.
for (const [name, target] of Object.entries(builds))
  if (target.browser && !artifacts.find((artifact) => artifact.name === name)?.browser)
    throw new Error(`Build ${name} produced no browser adapter`);
console.log(JSON.stringify(artifacts, null, 2));
