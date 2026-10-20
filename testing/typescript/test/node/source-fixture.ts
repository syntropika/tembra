import { copyFile, mkdir, mkdtemp, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

import type { TembraHost } from "@tembra/sdk";
import { Type, defineFacet, defineHook, local, method, native, opaque } from "@tembra/sdk";

export const Bias = local("test.source.bias", {
  version: 1,
  input: Type.Object({}),
  members: { read: method(undefined, Type.Integer()) },
});
export const Capability = native("test.source.capability@1");
export const Counter = local("test.source.counter", {
  version: 1,
  input: Type.Object({}),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
    hook: method(Type.Integer(), Type.Integer()),
    wait: method(undefined, Type.Integer()),
    value: opaque(Capability.native),
  },
});
export const Hook = defineHook("test.source.hook", {
  version: 1,
  strategy: "waterfall",
  input: Type.Integer(),
});
export async function sourceFixture() {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-source-"));
  await copyFile(
    new URL("fixtures/node-source/facet.ts.fixture", import.meta.url),
    path.join(root, "facet.ts"),
  );
  await writeFile(path.join(root, "middle.mjs"), "export {step,leafPayload} from './leaf.ts';");
  await mkdir(path.join(root, "node_modules/installed-dependency"), { recursive: true });
  await writeFile(
    path.join(root, "node_modules/installed-dependency/package.json"),
    '{"name":"installed-dependency","version":"1.0.0","type":"module","exports":"./index.js"}',
  );
  await writeFile(
    path.join(root, "node_modules/installed-dependency/index.js"),
    "export const bonus=3;",
  );
  await updateSource(root, 1);
  return root;
}
export async function updateSource(root: string, step: number) {
  await writeFile(
    path.join(root, "leaf.ts"),
    `export const step:number=${step};export const leafPayload:number[]=Array.from({length:50000},()=>step);`,
  );
}
export async function installBias(host: TembraHost) {
  host.registerHook(Hook);
  host.registerOpaque(Capability);
  await host.install(
    defineFacet({
      name: "source-native-bias",
      uses: [],
      setup(_scope, declaration) {
        declaration.serve(Bias, () => ({ state: {}, methods: { read: () => 10 } }));
      },
    }),
  );
}
