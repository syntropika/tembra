# Guide

Tembra manages typed objects, their dependencies and reactive state in one Rust core. Start with
[the counter tutorial](tutorial.md), use this guide to integrate a platform, and consult the
[specification](spec.md) for guarantees. The [architecture](architecture.md) explains module
boundaries and ownership.

## Choose an entry point

| Application | Entry point | Execution environment |
| --- | --- | --- |
| Rust | `tembra`, with `native` for `tembra::native::run` | Typed providers on the application's Rust driver. |
| Node.js | `@tembra/sdk/node` | Node callbacks connected to Rust through Node-API. |
| Browser | `@tembra/sdk/browser` | Browser JavaScript connected to Rust through `wasm-bindgen`. |
| Rust embedding JavaScript | Optional `tembra-executor-js` | Embedded V8 controlled by the Rust application. |

Use `@tembra/sdk` for shared TypeScript contracts, facets, hooks and schemas. The Node binding uses
Node's existing runtime; it does not include the embedded V8 executor.

## Objects and references

A contract describes an object's input and members. A synchronous provider factory creates its
implementation when needed. Contract name, normalized input and principal determine identity
within a Host. Supported defaults and input cleaning happen before identity comparison.

Acquisition creates a claim. Rust `host.get(contract, input).await` returns a ready typed reference.
Clones share that claim; acquire again for an independent lifetime. Dropping the final clone releases
it and schedules cleanup. In TypeScript, call `reference.release()` explicitly. Releasing a claim
does not promise immediate disposal: provider policy, dependencies and active work also matter.

Local contracts can carry native values through checked adapters. These capabilities are local;
they cannot be sent over RPC. Remote contracts use portable schemas, placement and named variants.
The selected variant determines which members a reference exposes. A declaration describes the
contract; it does not register an implementation.

## Rust applications

The [counter contract](../examples/rust/src/counter.rs) uses
`#[tembra::contract(local, name = "example.counter", version = 1)]`. The macro generates its input,
provider trait and typed reference. Import the application API through `tembra::prelude::*`.

A factory receives normalized input, principal and a borrowed `Declaration`. `host.serve` connects
its typed contract to the factory. `declaration.get(contract, input)` records an object dependency
synchronously; the core manages its readiness. Declare state and resources in the factory, and
keep asynchronous initialization in their managed initializers. External factory effects are not
rolled back if activation fails.

Create a writable state cell with `declaration.state("count", 0_u32)` and expose its `readable()`.
`cell.change(&context, |draft| ...)` validates the candidate and affected internal derivations
before adoption. Refusal leaves those pending local changes unpublished; external callback effects
are outside that guarantee. Pass an existing call context explicitly for nested calls:

```rust
let dependency = self.dependency.with_context(&context).read().await?;
```

This pattern is exercised by the [typed API tests](../testing/rust/tests/tembra/api_integration.rs).
`host.root(...)` creates an application scope, and `context.get(...)` acquires a scoped dependency.
Retaining a context clone does not extend that scope after the owning operation settles.

A `Readable` and its subscriptions are weak views. Keep the reference alive while observing state,
then unsubscribe and drop it. A snapshot retains its immutable historical value and revision.
The [state example](../examples/rust/basics/state.rs) demonstrates derivation and observation;
the [facet example](../examples/rust/facets/install.rs) groups typed registrations with `NativeFacet`.

`tembra::native::run` creates a current-thread runtime and awaits `host.close()` after its callback
returns, including on callback failure. Call it outside an already running Tokio runtime. Applications
with their own scheduler can construct `Host` with a driver and must await shutdown themselves.

The typed Rust facade covers ordinary providers, references, state and native facets. Advanced
registry, hook and executable-reload integration still uses the corresponding Rust core and adapter
APIs; these are not an additional typed facade demonstrated by the beginner examples.

## TypeScript Hosts

Node and browser expose the same high-level operations:

| Operation | Responsibility |
| --- | --- |
| `await createHost(options)` | Create one Rust Host through its platform binding. |
| `await host.install(facet)` | Install a facet defined through JavaScript callbacks. |
| `await host.load(moduleOrArtifact, selectedExport?)` | Prepare and install a dynamic facet; the export defaults to `default`. |
| `await host.reload(facets, options?)` | Prepare replacements and ask Rust to commit a compatible execution version. |
| `await host.get(contract, input)` | Acquire a reference and await readiness. |
| `host.hold(contract, input)` | Acquire immediately; await the reference's `ready` before use. |
| `reference.release()` | Release the acquisition's claim. |
| `await host.close()` | Await managed shutdown and cleanup. |

Acquisition defaults to `state: true`. Passing `{ state: false }` requests shape-only readiness;
it neither bypasses provider activation nor guarantees hydrated state reads. Use a state-demanding
acquisition when its initial readable values must be ready.

The maintained [contract](../examples/typescript/counter/contract.ts) and
[callback facet](../examples/typescript/counter/facet.ts) are shared by both platforms. The following
flow uses the same imports as their [Node entrypoint](../examples/typescript/counter/node.ts):

```typescript
import { createHost } from "@tembra/sdk/node";
import { Counter } from "./contract.ts";
import { counterFacet } from "./facet.ts";

const host = await createHost({ name: "counter-example" });
try {
  await host.install(counterFacet);
  const counter = await host.get(Counter, {});
  try {
    console.log(await counter.increment());
    console.log(counter.count.value);
  } finally {
    counter.release();
  }
} finally {
  await host.close();
}
```

In a browser, import `createHost` from `@tembra/sdk/browser`. Node options include `moduleBase`, the
base directory for relative source paths. Browser options include `baseURL` and optional WASM asset
initialization. Consult the [Node](../packages/sdk/src/node/index.ts) and
[browser](../packages/sdk/src/browser/host.ts) declarations for custom distribution options.

Factories passed to `declaration.serve` are synchronous. Declare cells and resources there; put
asynchronous work in a cell's `initialize` callback or a resource's `open` callback. Callback facets
and loaded facets use the same bridge and Rust object model. Remove state subscriptions when done.
TypeScript types alone do not validate untrusted data or issue native capabilities.

### Module loading and reload

The [module example](../examples/typescript/modules/node.ts) retains its counter reference and
state across reload. This excerpt loads a Node source module:

```typescript
await host.load(new URL("./facet.ts", import.meta.url));
await host.reload([new URL("./facet.ts", import.meta.url)]);
```

`load` accepts a string, URL or `EsmArtifactV1` value. Select a named export with
`host.load(module, "myFacet")`. Reload batches can contain module references, `{ module, selected }`
entries, artifact entries or callback facet definitions.

| Delivery | Node | Browser |
| --- | --- | --- |
| Module path or URL | `node-source-v1`: Jiti transforms a fresh local module graph in Node. | `browser-module-v1`: native ESM import with browser caching. |
| `tembra-esm-v1` artifact | Validated closed ESM evaluated by native Node ESM. | Validated closed ESM evaluated by native browser ESM. |

Node source entries accept local `.ts`, `.mts`, `.cts`, `.js` and `.mjs` files. This profile uses
CommonJS-style transformed evaluation, including wrapper globals; it does not promise native ESM
semantics. Top-level await, JSX and `tsconfig` aliases are unsupported. Installed package resolution
is supported, but the SDK does not install dependencies. Builtins and native packages retain their
ordinary runtime caching. The transform is not a typechecker.

Browser modules must already be executable JavaScript, for example served by Vite. Import maps,
CORS, CSP and transitive dependency caches remain browser responsibilities. Repeating the same URL
can return its cached module; `reload` does not rewrite URLs or discover transitive edits. Supply
updated URLs and dependency graphs through the application's delivery strategy.

Closed artifacts retain format, hash, export and ESM validation. They have no external imports or
dynamic dependency resolution. Produce them with `@tembra/sdk/tooling`; they are an explicit delivery
option, not a required envelope around every platform module.

### Browser embedding controls

Use `CreateBrowserHostOptions.wasm` for a matching preinitialized WASM module or an async loader,
and `wasmInput` to configure the packaged initializer. `baseURL` controls module resolution;
`consumerLinger` configures the Rust consumer cache. Advanced application operations remain on the
Host: `root` and `cancel` for issued contexts, `connect` for WebSocket readiness/status/closure,
and `diagnostics` for retained failures and ownership observations. Release references and remove
subscriptions explicitly; await `close` for managed shutdown and wrapper disposal.

Supplying a module still creates a new Host; it does not adopt an externally owned raw Host. A custom
application loader can pass an already evaluated `FacetDefinition` to `install` or `reload`.
The application then owns evaluation, caching and retention; registration does not charge those
external evaluations to the SDK's module-loader budget.

The old client/executor classes and raw `free()` handles are not an additional public API. Consumer
state still exposes `value` and `subscribe`; raw revision snapshots, per-readable hydration waits
and operation/reset event streams are not exposed. The [Browser interface decision](architecture.md#browser-public-interface)
explains this scope and the criteria for any future typed observation capability.

### Hooks, registries and opaque capabilities

Rust managed consumers can prepare hook metadata in their normal factory with
`hooks.declare_waterfall(declaration.core_mut(), "before-call", build_definition)` and retain the
returned `PreparedWaterfall<A>`. Its `run(input, context.core())` delegates to the existing runner.
Series, bail, middleware and parallel have corresponding typed declaration handles, including the
ordinary optional-terminal and `run_then` forms. Producers run during declaration rather than the
first call; a replacement implementation prepares its own metadata. Handles do not keep the
implementation alive and refuse use through an expired Context or an ended owner.

For middleware, select the actual operation from the current Context's registry on each step and
pass it to `run_native` or `run_portable`. These convenience methods reuse the declared checked types
while constructing an adapter for that supplied operation. Preserve explicit portable encode/decode
callbacks; do not retain an operation selected during startup as a substitute for current selection.
See the [executable Rust declaration example](../crates/hooks/src/prepared.rs).

Portable hooks use `defineHook`, `host.registerHook`, `registerHook` and `hookHandler`. Generic binding
adapters connect their checked arguments and results to the Rust hook manager. `FacetScope.runHook`
invokes a pipeline explicitly under an issued context; registering a hook does not intercept
unrelated methods. Follow the complete [normalization example](../examples/typescript/hooks/normalize-name.ts),
which uses a host-lifetime provider to keep its contribution active.

Use `defineRegistry` for portable contribution data and an explicit conflict policy. Facet and object
declarations register contributions; `host.registry(definition)` obtains a managed view. Retained
snapshots remain associated with their execution version.

A local JavaScript capability needs explicit nominal registration:

```typescript
import { native, opaque } from "@tembra/sdk";

const Session = native<{ readonly token: symbol }>("example.session@1");
host.registerOpaque(Session);
const sessionMember = opaque<{ readonly token: symbol }>(Session.native);
```

Use `sessionMember` in a local contract's `members`. The provider exposes it through an issued
`self.resource(...)` handle in its `opaque` map. A plain object cannot substitute for that handle.
The binding preserves JavaScript identity and verifies resource ownership. Registration does not
authorize arbitrary Rust native types or portable RPC transport. Crossing a Rust-specific native
type boundary requires that type's Rust adapter.

### Diagnostics and memory

Inspect `host.diagnostics()` during operation and after `close()`. Node diagnostics include cumulative
loader admission, generations, bridge owners and process memory observations. Browser diagnostics
include module admission and binding state. Warnings and structured budget errors indicate when
explicit environment replacement is needed. Recreating a Host in the same environment does not
replenish its module budget.

Retiring a Tembra generation releases managed ownership after old work settles. This does not
guarantee physical JavaScript module unloading. Finite admission limits evaluations; it is not a
heap cap on trusted application code. See [loader policy](spec.md#module-admission-and-memory)
for limits and [environment supervision](architecture.md#environment-supervision) for replacement.
Replacing an environment creates a new Host; it does not preserve its in-memory references or state.

### Supervise a child Host

`createHostManager` uses `forkDriver` in Node or `webWorkerDriver` in a browser. After `start`, retain
the returned instance to inspect `status()`, `failure()` and `activity()`, and await its `closed`
Promise to observe the tracked teardown attempt. The first terminal failure remains available;
normal shutdown leaves it undefined. A completed teardown attempt does not prove that every closer
succeeded, so also handle errors from `stop()`.

Rust RPC supplies liveness detection, with default pings every 10 seconds and a 30-second deadline
without peer traffic, subject to scheduler progress. Transport activity includes those pings; it
does not measure business activity. A suspended or blocked runtime can time out after scheduling
resumes. The manager keeps the environment alive even without application references until stop,
replacement, parent shutdown or failure teardown. It does not apply idle disposal or an `unref`
policy. See [environment supervision](architecture.md#environment-supervision) for ownership details.

## Cleanup and replacement guarantees

Await `host.close()` to join managed shutdown and resource cleanup. Cancellation signals do not
prove a foreign callback has stopped or undo a completed operation. Cleanup can report failures
after earlier effects have occurred.

Compatible dynamic replacement retains object identity, surviving references and compatible state.
Old call roots retain their execution version; new roots use the committed version. State migration
and resource adoption can refuse a replacement. Preparation and arbitrary external effects are
outside the core commit guarantee. Native facets remain fixed for their Host lifetime. Retained
state is in memory; use application storage when it must survive environment shutdown.

## Run the maintained examples

The [Rust tutorial](tutorial.md) starts with `just run-rust-example`. Add one concept at a time with
`just run-example local`, `just run-example state` and `just run-example facets`.

For TypeScript examples in the checkout, install dependencies and build the SDK and Node binding:

```sh
just install-ts
just build-ts
just build-node-binding
bun run --filter @tembra/example-typescript node
bun run --filter @tembra/example-typescript modules
bun run --filter @tembra/example-typescript hooks
```

For the browser example:

```sh
just build-browser-wasm
bun run --filter @tembra/example-typescript dev
```

Vite serves the TypeScript application directly during development. Open its printed URL; the
default page installs a callback facet and `/?module` loads it as a browser module. No example
application build is needed before starting the server. The SDK and WASM binding assets must exist.

## Build and development tools

See the [README](../README.md) for prerequisites. `@tembra/sdk/tooling` provides contract generation,
typechecking, bundling and file watching outside the runtime. Its
[configuration types](../packages/sdk/src/tooling/types.ts) describe targets, outputs and adapters.
Build through the programmatic API in a Bun script:

```typescript
import { build, loadConfig } from "@tembra/sdk/tooling";

const config = await loadConfig("./tembra.config.ts");
const result = await build(config);
for (const output of result.artifacts) {
  console.log(output.artifactPath);
}
```

The maintained [artifact build script](../examples/typescript/modules/build-artifacts.ts) uses this
API and runs with `bun run --filter @tembra/example-typescript artifacts`. Create the configuration
first. `build` publishes checked artifacts; installation is a separate Host operation.

For development, the application owns the session and its shutdown:

```typescript
import { once } from "node:events";
import { dev, loadConfig } from "@tembra/sdk/tooling";

const session = await dev(await loadConfig("./tembra.config.ts"), {
  onBuild: (result) => console.log(result.artifacts),
  onError: console.error,
});
try {
  await session.ready;
  await once(process, "SIGINT");
} finally {
  await session.close();
}
```

Supply explicit installation capabilities through `DevOptions.hosts` to deliver updates. Without
those capabilities, builds can complete while delivery remains pending. A completed build does not
prove that a running Host installed the artifact. Cancellation requests cooperative shutdown;
`session.close()` joins the session's owned work and adapter cleanup. Descriptor generation also
remains programmatic through `generateBindings`; configured external Rust generators are invoked
only when that generation is requested.
