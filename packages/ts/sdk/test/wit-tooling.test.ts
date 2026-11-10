import { expect, test } from "bun:test";
import { access, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { build } from "../src/tooling/index.ts";
import { NodeArtifact } from "@syntropika/tembra-engine-node";
import { readFile } from "node:fs/promises";

test("WIT builds produce Wasm data and editor declarations, reject wrong values and invalidate after contract edits", async () => {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-wit-"),
  );
  const contract =
    "package example:typed; interface calculation { record input { value: u32 } read: func(input: input) -> u32; } world calculator { export calculation; }";
  const source =
    'import type {calculation} from "./contract.wit"; const value: calculation.Input = {value:42}; export default {name:"typed",apiVersion:1,services:{calculation:{methods:{read:()=>value.value}}}} as const;';
  try {
    await writeFile(path.join(directory, "contract.wit"), contract);
    await writeFile(path.join(directory, "orb.ts"), source);
    const prepare = async () =>
      (
        await build({
          root: directory,
          builds: { typed: { entry: "orb.ts", wit: "contract.wit" } },
        })
      )[0]!;
    const first = await prepare();
    const decoded = new NodeArtifact(new Uint8Array(await readFile(first.path)));
    expect(JSON.parse(decoded.description())).toMatchObject({ name: "typed", kind: "source" });
    await access(path.join(directory, "contract.d.wit.ts"));
    await access(path.join(directory, ".tembra"));
    expect(first.path).toEndWith(".wasm");
    await writeFile(path.join(directory, "orb.ts"), source.replace("value:42", 'value:"wrong"'));
    await expect(prepare()).rejects.toThrow("not assignable");
    await writeFile(
      path.join(directory, "contract.wit"),
      contract.replace("value: u32", "value: string"),
    );
    await writeFile(path.join(directory, "orb.ts"), source);
    await expect(prepare()).rejects.toThrow("not assignable");
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("WIT resources support typed using declarations without author-maintained bindings", async () => {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-wit-dispose-"),
  );
  try {
    await writeFile(
      path.join(directory, "contract.wit"),
      `
      package example:disposable;
      interface probe { inspect: async func() -> string; }
      world app {
        import wasi:filesystem/types@0.3.0;
        import wasi:filesystem/preopens@0.3.0;
        export probe;
      }
    `,
    );
    await writeFile(
      path.join(directory, "orb.ts"),
      `
      import {getDirectories} from "wasi:filesystem/preopens@0.3.0";
      export default {name:"disposable",apiVersion:1,services:{probe:{methods:{
        async inspect() {
          using directory = getDirectories()[0]![0];
          using file = await directory.openAt({}, "bytes.bin", {}, {read:true});
          return (await file.stat()).size.toString();
        }
      }}}} as const;
    `,
    );
    const [artifact] = await build({
      root: directory,
      builds: { disposable: { entry: "orb.ts", wit: "contract.wit", world: "app" } },
    });
    expect(artifact!.bytes).toBeGreaterThan(0);
    const value = new NodeArtifact(new Uint8Array(await readFile(artifact!.path)));
    expect(JSON.parse(value.description()).name).toBe("disposable");
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

test("filesystem timestamp types do not grant or expose callable clocks", async () => {
  const directory = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-wit-types-only-"),
  );
  const clock = "wasi:clocks/system-clock@0.3.0";
  try {
    await writeFile(
      path.join(directory, "contract.wit"),
      `
      package example:type-authority;
      interface inspector { inspect: func() -> bool; }
      world app {
        import wasi:filesystem/preopens@0.3.0;
        export inspector;
      }
    `,
    );
    const source = `import type {Instant} from "${clock}";
      const stamp: Instant = {seconds:0n,nanoseconds:0};
      export default {name:"inspector",apiVersion:1,services:{inspector:{methods:{inspect:()=>stamp.nanoseconds===0}}}} as const;`;
    await writeFile(path.join(directory, "orb.ts"), source);
    const prepare = async () =>
      (
        await build({
          root: directory,
          builds: { inspector: { entry: "orb.ts", wit: "contract.wit", world: "app" } },
        })
      )[0]!;
    const artifact = await prepare();
    const decoded = new NodeArtifact(new Uint8Array(await readFile(artifact.path)));
    const planned = JSON.parse(
      decoded.request(
        JSON.stringify({
          kind: "install",
          name: "inspector",
          apiVersion: 1,
          requires: {},
          grants: { filesystem: [{ provider: "data", mount: "/data", writable: false }] },
          filesystem: ["data"],
          network: [],
        }),
      ),
    );
    expect(planned.error).toBeUndefined();
    expect(planned.ok).toBeDefined();
    // The same generated declaration provides types, while a runtime function needs an import.
    await writeFile(
      path.join(directory, "orb.ts"),
      source
        .replace(`import type {Instant} from "${clock}";`, `import {now} from "${clock}";`)
        .replace("const stamp: Instant = {seconds:0n,nanoseconds:0};", "const stamp = now();"),
    );
    await expect(prepare()).rejects.toThrow(`the Orb contract does not import ${clock}`);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
