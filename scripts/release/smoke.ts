import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

const [archives, directory] = process.argv.slice(2);
if (!archives || !directory)
  throw new Error("Usage: bun scripts/release/smoke.ts <npm-artifacts> <empty-project>");
mkdirSync(directory, { recursive: true });
writeFileSync(
  path.join(directory, "package.json"),
  JSON.stringify({ private: true, type: "module" }),
);
const packages: { name: string; filename: string }[] = JSON.parse(
  readFileSync(path.join(archives, "packages.json"), "utf8"),
);
execFileSync(
  "npm",
  [
    "install",
    "--ignore-scripts",
    "--omit=optional",
    "--no-audit",
    "--no-fund",
    "--no-package-lock",
    ...packages
      .filter((item) => item.name !== "@tembra/react-native")
      .map((item) => path.resolve(archives, item.filename)),
  ],
  { cwd: directory, stdio: "inherit" },
);
const script = `
import {readFileSync} from 'node:fs';
import {createHost as nodeHost} from '@tembra/sdk/node';
import init from '@tembra/engine-wasm';
await init({module_or_path:readFileSync(new URL(import.meta.resolve('@tembra/engine-wasm/tembra_bg.wasm')))});
const {createHost:browserHost}=await import('@tembra/sdk/browser');
for (const create of [nodeHost,browserHost]) {
  const host=await create();
  await host.install({name:'packed',apiVersion:1,services:{packed:{methods:{read:()=>42}}}});
  const ref=await host.get('packed','main');
  if(await ref.read()!==42)throw Error('Packed runtime failed');
  ref.release();await host.close();
}
console.log('Packed Node and WASM runtimes passed');
`;
const entry = path.join(directory, "smoke.mjs");
writeFileSync(entry, script);
for (const runtime of ["node", process.execPath])
  execFileSync(runtime, [entry], { cwd: directory, stdio: "inherit" });
