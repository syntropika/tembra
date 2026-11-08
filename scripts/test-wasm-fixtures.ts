import { mkdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { parseArgs } from "node:util";
import { build } from "../packages/ts/sdk/src/tooling/index.ts";

const { values } = parseArgs({
  options: { out: { type: "string" }, provider: { type: "string" } },
});
if (!values.out) throw new Error("Supply --out with an external qualification artifact directory");
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
  ["online", calculator.replace('name: "calculator"', 'name: "online"'), "online-calculator"],
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
    minify: false,
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
    `import {orb} from ${guest};
export default orb({name:${JSON.stringify(name)},apiVersion:1,services:{files:{methods:{
read:async(_e:unknown,_c:unknown,path:string)=>tembra.filesystem!.readText(path),
write:async(_e:unknown,_c:unknown,path:string,value:string)=>tembra.filesystem!.writeText(path,value),
}}}});`,
  );
  builds[name] = {
    entry: `storage/${name}.ts`,
    wit: "storage/storage.wit",
    world: "storage",
    minify: false,
  };
}
if (values.provider)
  builds.arithmetic = {
    entry: path.resolve(values.provider),
    wit: "calculation/contract.wit",
    world: "arithmetic-provider",
    apiVersion: 1,
  };
const artifacts = await build({ root: sources, outDir: directory, builds });
for (const artifact of artifacts) {
  await writeFile(path.join(directory, `${artifact.name}.wasm`), await readFile(artifact.path));
}
console.log(JSON.stringify(artifacts, null, 2));
