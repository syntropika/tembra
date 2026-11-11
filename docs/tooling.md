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
2. generates the TypeScript declarations from WIT;
3. typechecks the entry, its imports and project-selected ambient declarations against the guest globals;
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

### Prepare a compiled Orb for browsers

Set `browser: true` on a compiled target to emit its application adapter alongside the artifact:

```ts
const artifacts = await build({
  builds: {
    arithmetic: {
      entry: "provider.wasm",
      wit: "contract.wit",
      world: "arithmetic-provider",
      apiVersion: 1,
      browser: true,
    },
  },
});
```

The build verifies the actual component signatures, generates its browser bindings with JCO,
and instruments its core modules for instruction and memory limits. It writes the adapter under
the target's `browser/` directory. The application imports and registers that module:

```ts
import arithmetic from "./dist/tembra/arithmetic/browser/index.js";
import { createHost } from "@syntropika/tembra-sdk/browser";

const host = await createHost({ isolated: { components: [arithmetic] } });
await host.load("./dist/tembra/arithmetic/orb.<sha256>.wasm");
```

The adapter is trusted application code, prepared by the application's build. An Orb cannot name
a JavaScript file for the host to execute. The browser computes the digest of the actual artifact
and accepts a compiled Orb only when an application-registered adapter matches those exact bytes.
Changing the component or its packaged contract requires preparing its adapter again.

JCO, the component parser and the instrumenter stay in tooling. Browser execution receives the
prepared bindings and core modules; each installation gets fresh memories, handles, grants and
execution budgets. Prepared components require the browser's JavaScript Promise Integration
support. Raw compiled artifacts without a registered adapter are refused.

Source JS/TS targets do not use this step. Their closed scripts run through the shared dquickts
interpreter and remain dynamically loadable without generating per-Orb browser bindings.

### Keyed dependency references

A plain compiled WIT import calls the provider's `main` object. To select an object by key,
import a reference view: a resource whose methods mirror the provider's functions, plus an
asynchronous acquisition function. For example, a provider exporting `counter.add` and
`counter.who` can be addressed through:

```wit
interface counter-ref {
    resource counter {
        add: func(amount: u32) -> u32;
        who: func() -> string;
    }
    get: async func(key: list<string>) -> result<counter, string>;
}
```

Declare the view's target interface when packaging the compiled component:

```ts
const artifacts = await build({
  builds: {
    router: {
      entry: "router.wasm",
      wit: "keyed.wit",
      world: "router",
      apiVersion: 1,
      references: {
        "example:keyed/counter-ref@1.0.0": "example:keyed/counter@1.0.0",
      },
      browser: true,
    },
  },
});
```

At installation, select the provider for the **view import binding** as usual:

```ts
await host.load(artifacts[0]!.path, {
  requires: { "example:keyed/counter-ref@1.0.0": { orb: "keyed", apiVersion: 1 } },
});
```

Rust packaging uses `tembra_executor_wasm::ComponentRoles` as the last argument to
`Contract::component_artifact`, with the same `references` mapping and optional `contributions`.

The official WIT bindings expose acquisition and methods directly inside the compiled guest:

```rust
async fn update(session_id: String) -> Result<u32, String> {
    let personal = counter_ref::get(vec!["personal".into(), session_id.clone()]).await?;
    let work = counter_ref::get(vec!["work".into(), session_id]).await?;
    work.add(1);
    Ok(personal.add(1))
}
```

Tembra checks the role against the real WIT, derives the provider requirement and verifies its
signature at installation. Method names, parameter names, types, results and async declarations
must match the provider. Methods carry data; streams and transferred resources are outside this
view profile. Without the explicit role, the same resource declaration has ordinary WIT resource
semantics and cannot select kernel objects.

Acquisition applies the installed requirement and the admitted root's principal. Equal addresses
share an object according to its lifecycle policy; different keys or principals identify different
objects. Dropping a view releases its claim when the callback settles. Ending the root expires
all its claims, including handles retained in guest globals. A refused acquisition returns the
declared WIT error; a stale handle cannot act for a later root.
Node and mobile may finish an abandoned readiness wait before its callback ends; if the guest
no longer observes the reply, Tembra releases the claim instead of publishing it into that root.

Keys are opaque to Tembra. The application defines their meaning and authorizes configurations;
a key grants no permission and creates no separate Wasm heap. Native and prepared browser hosts
use this same model. See the [compiled fixture](../crates/executor-wasm/rust-keyed-router/src/lib.rs).

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

The build uses the ECMAScript standard library plus the guest's `console` and `tembra` declarations.
Browser and Node globals such as `setTimeout`, `Buffer`, `fetch` and `URL` are unavailable, even if
the application project enables their type libraries. Import the guest HTTP helper explicitly.
Ambient `.d.ts` files selected by the project configuration remain available for build-time constants.
