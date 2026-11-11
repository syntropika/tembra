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
Canonical imported interface declarations are shared under the build root's `.tembra/imports/`,
so several contracts in one editor project can consume the same interface without duplicate
declarations. The shared graph includes the interface's transitive types. A build rejects
different definitions of the same canonical interface identity before writing editor files.
Source targets sharing a WIT file must select the same world so the editor alias stays unambiguous.
Compiled targets do not generate TypeScript declarations.
Set `customConditions: ["tembra-wasm"]` and `allowArbitraryExtensions: true` in the
source project's `tsconfig.json` to use the same guest API and direct WIT types in the editor.

### Infer implementations from the contract

Keep the contract and implementation as the two author-owned source files. `WitMethods`
contextually types every method's environment, context, arguments and result; no argument
annotations, copied record types or handwritten bindings are needed:

```wit
package example:counter@1.0.0;
interface counter {
    add: func(amount: u64) -> u64;
    read: func() -> u64;
}
world app { export counter; }
```

```ts
import { orb, type Declaration, type WitMethods } from "@syntropika/tembra-sdk";
import type { counter } from "./contract.wit";

function create(self: Declaration) {
  return self.state(0n);
}

export default orb({
  name: "counter",
  apiVersion: 1,
  services: {
    counter: {
      create,
      methods: {
        add(total, context, amount) {
          return total.update(context, (value) => value + amount);
        },
        read(total) {
          return total.value;
        },
      } satisfies WitMethods<typeof counter, ReturnType<typeof create>>,
    },
  },
});
```

Missing or extra methods and incorrect results fail the build. Here `amount` and the state
are `bigint` because the WIT contract uses `u64`. WIT remains canonical: TypeScript `number`
alone cannot distinguish a WIT `u32`, `s32` or `f64`. The Orb declaration retains runtime
policy such as state creation and lifetime, which WIT does not express.

Imported interface types also use their canonical WIT identities, including other Orbs:

```ts
import type * as Arithmetic from "example:integrated/arithmetic@1.0.0";

// In a service factory whose world imports arithmetic:
const arithmetic = self.get<typeof Arithmetic>("example:integrated/arithmetic@1.0.0", "main");
```

The same generated declarations serve the build and editor. The managed `.d.wit.ts` alias
loads their canonical module names automatically; no `paths` entries or JCO-generated
namespace names are required. These Orb imports are type-only; acquisition still goes
through `self.get` or `context.get`, and the installer chooses the provider. Imported types
grant no authority. Builds reuse declarations for targets sharing one contract and leave
unchanged editor files untouched.

The pinned JCO generator cannot distinguish every pair of versions of one imported interface.
If it collapses different canonical identities into one declaration file, the build rejects
the ambiguous declarations instead of assigning one version's types to another.

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

### Resources and byte streams between browser components

Prepared compiled components use the same bounded WIT capability profile as the native executor:

- A constructor or factory may return owned resources as its result, including resources nested
  in records, lists, tuples, simple options, variants and results. Its methods borrow `self`;
  the provider keeps each resource in its own component instance.
- An asynchronous freestanding function may return `stream<u8>` as its complete result. The
  receiver pulls bounded chunks rather than collecting the body before returning the stream.

For example:

```wit
interface sessions {
    resource session {
        constructor(seed: u64);
        add: func(value: u64) -> u64;
    }
    record batch {
        primary: session,
        optional: option<session>,
        extra: list<session>,
        seed: u64,
    }
    open: func(seed: u64) -> result<batch, string>;
}

interface frames {
    read: async func() -> stream<u8>;
}
```

Prepare and register both components as above, then bind the consumer's imports with ordinary
`requires`. No additional author API is needed: generated WIT bindings construct, borrow and
drop resources, and read or drop streams. Resource ownership retains the exact provider claim,
including its permissions; a reload cannot redirect an old resource to a new component instance.
A guest drop releases its owner after the importing callback settles, and a physical borrower
keeps the resource alive until that borrow ends.

Stream publication and physical completion are separate. Several stream heads can be acquired
before any body is read. The producer retains its admitted invocation and deadline while its
writer is running. Cancellation, normal root end and reader drop close the stream; physical
completion remains tracked after an early drop. A producer trap is reported to the host and
quarantines that producer revision. Overlapping callbacks share the instance's remaining fuel
budget until the group physically settles, so publishing another stream never replenishes fuel.
Expiry rejects the observed operation and revokes its authority immediately. It does not prove
that the guest exited: invocation and resource pins remain until the actual task and its host
operations settle. If a quarantined task never acknowledges exit, shutdown keeps waiting.

These capabilities travel only through the private component transport. Ordinary data-only
host calls refuse a resource or stream result and close the complete batch. Result attachments
are checked against their nominal WIT identities and consumed exactly once; malformed data,
duplicate or unused attachments and failed delivery close every owner. Owned arguments, nested
streams, non-byte stream elements and arbitrary futures remain outside this profile.

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
