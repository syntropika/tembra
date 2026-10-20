# Guide

Tembra manages typed objects, their dependencies and reactive state in one Rust core. Start with
the [counter tutorial](tutorial.md), then use this guide to choose an integration and understand
ownership. The [specification](spec.md) records the behavioral rules in more detail.

## Objects and references

A contract describes an object's input and members. A provider registers a synchronous factory;
the host invokes it when that object is needed. Within a host, contract name, normalized input
and principal determine object identity. Supported defaults and input cleaning happen before
identity comparison, so equivalent inputs can select the same object.

Acquiring a reference creates a claim on that object. In Rust, `host.get(contract, input).await`
returns a ready typed reference. Cloning a reference shares its claim; it does not create an
independent acquisition. Explicitly releasing that claim releases every clone. Acquire again
when another owner needs an independent lifetime.

Object lifetime also depends on provider policy, dependencies and active work. Releasing one
reference does not promise immediate object disposal. See the maintained
[identity and lifecycle tests](../crates/core/tests/lifecycle.rs).

## Contracts and language boundaries

Use `#[tembra::contract(local, name = "example.counter", version = 1)]` to declare a local Rust
contract. The macro generates its input, provider trait and typed reference. The
[counter contract](../examples/rust/src/counter.rs) shows methods and observable state together.

Local contracts can carry native values or opaque capabilities through an embedding's checked
adapters. Native values are local capabilities, not values that can be sent over RPC. Remote
contracts use portable schemas, placement and named variants; the selected variant determines
which members a reference exposes. A schema declaration does not register an implementation.

TypeScript authors use `@tembra/sdk` to declare the same contract model:

```ts
import { hostNamed, method, remote, Type } from "@tembra/sdk";

const Counter = remote("example.counter", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("service"),
  variants: {
    live: {
      state: { count: Type.Integer() },
      methods: { read: method(undefined, Type.Integer()) },
    },
  },
});
```

Here `undefined` means no argument payload; `Type.Null()` would declare a present null value.
The [descriptor emitter](../packages/sdk/examples/emit-counter.ts) is a complete source example.
Portable inputs, method values and state are validated at their boundaries. TypeScript types
alone do not validate untrusted data or issue native capabilities.

## Factories, dependencies and state

A Rust factory receives the normalized input, principal and a borrowed `Declaration`. Use the
declaration to record state and object-owned dependencies. `declaration.get(contract, input)`
records a dependency synchronously; the core manages dependency readiness during activation.
Keep asynchronous initialization in managed state/resource initializers. Factories are not an
async setup hook, and arbitrary external effects performed in a factory are not rolled back.

Use `declaration.state("count", 0_u32)` for a writable `Cell<u32>`. Expose its `readable()` to
consumers. A synchronous `cell.change(&context, |draft| ...)` validates the candidate and affected
derived values before adopting the local state change. A refused candidate leaves those pending
local changes unpublished. This guarantee does not undo external effects inside the callback.

A `Readable` and its subscriptions are weak capabilities: retaining them does not retain an
object claim. Keep the reference alive while observing state, then unsubscribe and release it.
A snapshot retains an immutable historical value and revision; later changes do not rewrite it.
For derivation and subscription cleanup, run the
[state example](../crates/core/examples/state.rs):

```sh
just run-example state
```

## Contexts and cleanup

A method's `Context` carries its root, principal, execution version and cancellation scope.
Pass it explicitly when making a nested Rust call:

```rust
let dependency = self.dependency.with_context(&context).read().await?;
```

This pattern appears in the [typed facade tests](../crates/tembra/tests/api_integration.rs).
`host.root(...)` creates an application scope; `context.get(...)` acquires references owned by
that scope. Retaining a context clone does not extend its lifetime after the owning call settles.

Release references when their owner finishes. Await `host.close()` to join managed shutdown and
resource cleanup. The native runner in the tutorial does this after its callback settles, including
on callback failure. Cleanup can report errors after earlier effects have occurred. Cancellation
signals do not prove a foreign callback has stopped or undo a completed operation.

## JavaScript on native hosts and in browsers

Native JavaScript runs in the embedded V8 executor attached to a Rust host. It shares object
identity, state and execution ownership with Rust providers. The
[interop example](../crates/executor-js/examples/interop.rs) passes one native capability through
Rust and JavaScript methods on the same host:

```sh
just run-executor-example interop
```

V8 is an embedding environment; its globals are not Node.js globals. Hosts explicitly supply
trusted capabilities and adapters.

Browser JavaScript uses `@tembra/sdk/browser` over the Rust core compiled to WebAssembly. Build
the bindings with `just build-browser-wasm`, initialize the generated module, then pass it to
`BrowserClient`. The [browser entrypoint](../packages/sdk/src/browser/index.ts) contains a complete
consumer flow with connection readiness, reference readiness, release, close and wrapper cleanup.
The SDK does not download or instantiate the module for you. Connections do not automatically
reconnect. A disconnect can leave an already dispatched remote operation's outcome unknown;
reconcile application state before deciding whether another attempt is safe.

## Facets, hooks and reload

Facets group provider registrations and declared dependencies. Registries compose contributions
under an explicit conflict policy. Hooks add explicitly invoked pipelines through managed
methods; registering a hook does not intercept unrelated calls. Run the
[middleware example](../crates/hooks/examples/middleware.rs) with `just run-hooks-example`.

JavaScript reload prepares replacement code and checks compatibility before committing a new
execution version. Compatible replacement retains managed object identity, references and state.
Existing call roots keep their version; new roots use the committed version. State migration
and resource adoption can refuse a replacement. Preparation and arbitrary external effects are
outside the core commit guarantee. Native facets remain fixed for the process lifetime.

```sh
just run-executor-example javascript_reload
```

The [reload example](../crates/executor-js/examples/javascript_reload.rs) increments by one, reloads
an implementation that increments by ten, and observes eleven through the original reference.
Retained state is in memory; use application storage when it must survive process shutdown.

## Build and development tools

Build from this checkout using `just setup` and `just install-ts`; see the
[README](../README.md) for prerequisites. `@tembra/tooling` handles contract generation,
typechecking, bundling and file watching outside the runtime. Its
[configuration types](../packages/tooling/src/types.ts) describe targets, outputs and adapters.

```sh
just build-ts
just compile-tooling
packages/tooling/dist/bin/tembra build --config tembra.config.ts
packages/tooling/dist/bin/tembra dev --config tembra.config.ts
```

Create the configuration for your application first. The compiled executable embeds Bun.
`build` publishes checked artifacts; installation is a separate host operation. `dev` requires an
explicit trusted host adapter to apply updates. Without a connected host, delivery can remain
pending; a completed build does not establish that a running host installed the artifact.
