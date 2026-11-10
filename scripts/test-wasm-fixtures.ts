import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseArgs } from "node:util";
import { build } from "../packages/ts/sdk/src/tooling/index.ts";

const { values } = parseArgs({
  options: {
    out: { type: "string" },
    provider: { type: "string" },
    contributor: { type: "string" },
  },
});
if (!values.out) throw new Error("Supply --out with an external qualification artifact directory");
// The compiled hook contributor is part of every hook fixture set, so its tests never skip.
if (!values.contributor)
  throw new Error("Supply --contributor with the compiled hook contributor component");
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
  entry: path.resolve(values.contributor),
  wit: "hook-extension/contract.wit",
  world: "extension",
  apiVersion: 1,
  contributes: Object.fromEntries(
    ["guard", "audit", "choice"].map((hook) => [`example:hooks/${hook}@1.0.0`, -7]),
  ),
};
if (values.provider)
  builds.arithmetic = {
    entry: path.resolve(values.provider),
    wit: "calculation/contract.wit",
    world: "arithmetic-provider",
    apiVersion: 1,
  };
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
const artifacts = await build({ root: sources, outDir: directory, builds });
for (const artifact of artifacts) {
  await writeFile(path.join(directory, `${artifact.name}.wasm`), await readFile(artifact.path));
}
console.log(JSON.stringify(artifacts, null, 2));
