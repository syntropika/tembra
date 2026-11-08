# Build tools

Build tooling turns an orb into one immutable `.wasm` artifact before installation.
Import it from `@syntropika/tembra-sdk/tooling` and run it with Bun.
It needs the built Node engine (`just build-node`). It does not need the interpreter.

```ts
import { build } from "@syntropika/tembra-sdk/tooling";

const artifacts = await build({
  outDir: "dist/tembra",
  builds: {
    calculator: { entry: "orb.ts", wit: "contract.wit", world: "calculator" },
    arithmetic: {
      entry: "provider.wasm",
      wit: "contract.wit",
      world: "arithmetic-provider",
      apiVersion: 1,
    },
  },
});
```

Each build name produces `<outDir>/<name>/orb.<sha256>.wasm`. `build()` returns the name, path,
hash and byte count of each artifact. `outDir` defaults to `dist/tembra` under `root`.
The returned name identifies the build target. A source Orb keeps the literal installation
identity declared in its implementation, so a target such as `revised` can build a replacement
for the same Orb. Only compiled targets supply `apiVersion` in build configuration.

## Source orbs

A `.ts` or `.js` entry exports a default orb with literal `name` and `apiVersion` values.
For each source build, `build()`:

1. compiles the WIT file and selects `world`;
2. generates the official Jco type declarations;
3. typechecks the entry and its imports;
4. bundles the entry and its installed dependencies into one closed script;
5. writes the script and the encoded contract into the artifact.

The orb imports WIT types from the contract file directly:

```ts
import type { calculation } from "./contract.wit";
```

Generated declarations are written next to the WIT file in `.tembra/types/` and in a
`<name>.d.wit.ts` alias. These are managed files; keep them out of version control.
Source targets sharing a WIT file must select the same world so the editor alias stays unambiguous.
Compiled targets do not generate TypeScript declarations.
Set `customConditions: ["tembra-wasm"]` and `allowArbitraryExtensions: true` in the
source project's `tsconfig.json` to use the same guest API and direct WIT types in the editor.

Dependencies resolve with the `tembra-wasm` export condition. The build does not install
packages. It rejects Node builtins, native addons, dynamic module names and any import left
unresolved in the bundle. Minification is enabled by default; set `minify: false` to disable it.
`define` replaces identifiers at build time.

The bundle runs in the dquickts interpreter, not in Node or a browser. Node and DOM APIs are not
available. The orb receives filesystem and network access only through granted WASI imports.

## Compiled orbs

A `.wasm` entry is a compiled WebAssembly component. `build()` checks its exports against the
selected world, including imports, and packages it with the contract. The component carries no Tembra metadata, so
the build name becomes the orb name and the target must supply `apiVersion`.

## Install an artifact

Applications load artifacts by location. Default SDK loaders accept only `.wasm` files.

```ts
await host.load("dist/tembra/calculator/orb.<sha256>.wasm", {
  requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
});
```

Rust hosts use `tembra_executor_wasm::Engine::load`. See the
[Wasm orb example](../examples/wasm-orb/README.md) and
[Wasm orb execution](../crates/executor-wasm/README.md).

React Native uses Metro to bundle statically imported trusted orbs.
See the [React Native guide](react-native.md) for the mobile entrypoint.

See [build implementation](../packages/ts/sdk/src/tooling/build.ts) and
[dependency resolution](../packages/ts/sdk/src/tooling/resolver.ts).
