# Architecture

Tembra has one authoritative Rust object engine and platform-specific ways to execute facet code.
Applications use typed contracts and managed references. A loader prepares a facet; the core owns
what that facet contributes and how long its objects, state, resources and executions remain valid.
See the [guide](guide.md) for application usage and the [specification](spec.md) for behavioral rules.

## The module boundary

```mermaid
flowchart TD
    module[Module or explicit closed artifact] --> loader[Platform loader and execution profile]
    loader --> prepared[Checked prepared facet]
    prepared --> bridge[Platform registration bridge]
    callbacks[Callback facet installed directly] --> bridge
    bridge --> host[Tembra Rust Host]
```

A platform loader owns module acquisition, evaluation and its declared execution profile. It selects
an export and checks that the result is a facet. For a closed artifact, it also enforces that format's
hash and closed-module rules. A normal platform module does not need an artifact envelope.

A prepared facet is the boundary between evaluated code and managed execution. Bindings retain the
JavaScript callbacks and values required by that facet and register its declarations in Rust.
Registration and subsequent reload use core-owned execution versions. The core does not parse
TypeScript, resolve npm packages, maintain a JavaScript module cache or choose Jiti versus native ESM.

`install()` starts at the registration boundary with callbacks already in the application's runtime.
`load()` reaches the same boundary through a loader. `reload()` prepares candidates and delegates
compatibility, commit, state preservation and version retirement to the existing core machinery.
There is no TypeScript object engine or independent reload algorithm.

## Platform execution

| Platform | JavaScript execution | Rust connection | Module delivery |
| --- | --- | --- | --- |
| Node | Existing Node runtime | Node-API addon | Jiti source profile; native ESM for explicit closed artifacts. |
| Browser | Existing browser or Worker runtime | `wasm-bindgen` | Native ESM URLs and validated closed artifacts. |
| Rust with JavaScript | Optional embedded V8 | Executor's Rust adapter | Validated closed artifacts for the embedded executor. |

### Node

`@tembra/sdk/node` creates `tembra-node-binding` through Node-API. Its callback adapter connects
native facet setup, factories, methods, state, resources, hooks and opaque values to the same Rust
Host. It has no dependency on `tembra-executor-js`, `deno_core` or a second embedded V8.

The Node source loader uses Jiti 2.7.0 with a fresh preparation graph, disabled filesystem/module
caches and a synchronous transformed execution profile. It supports local JavaScript/TypeScript
modules and installed dependency resolution within that profile. CommonJS wrapper semantics are
explicit; the profile does not claim native ESM equivalence. The Jiti compiler bootstrap runs in a
compiler-only VM context to avoid mutating the application's Error configuration. Facet callbacks
execute in the existing Node realm and can retain ordinary JavaScript identity.

A `tembra-esm-v1` input uses native ESM after its closed artifact validation. This separate evaluator
serves a distinct explicit execution contract; passing that artifact does not opt into transformed
source semantics. Neither evaluator is an untrusted-code sandbox.

### Browser

`@tembra/sdk/browser` creates a Rust Host compiled to WASM and wraps its issued handles through
`wasm-bindgen`. Facet callbacks run in the browser's JavaScript engine. Module loading delegates
native import, import maps, URL resolution and dependency caching to the browser. Relative paths
use the configured `baseURL` or the ambient document/Worker base.

Development tools such as Vite serve executable JavaScript from TypeScript source. The Host does
not transpile browser `.ts` URLs. CSP and CORS remain platform constraints. Native cache entries
are not invalidated by `host.reload()`; an embedding controls how changed URLs and dependencies are
published. Closed artifacts follow their explicit validation and native ESM evaluation path.

### Optional Rust embedding

`tembra-executor-js` remains available to Rust applications that need embedded JavaScript. Its V8
executor adapts foreign facet code to the same Rust Host semantics. Keeping it independent allows
Rust embeddings to choose their engine without imposing that engine on Node or browser consumers.

## Browser public interface

The Browser entry point exposes one `createHost` facade over its matching Rust/WASM binding.
Separate client/executor/port construction is no longer a public integration path. Executor assembly
and binding ports are implementation details. The application operations remain available:
callback installation, module/artifact loading, compatible reload, reference acquisition, contexts,
cancellation, WebSocket connections and diagnostics. Resources, registries, portable hooks and
registered JavaScript opaque capabilities use the same underlying Host.

Embeddings retain control over WASM initialization through `CreateBrowserHostOptions.wasm` or the
packaged initializer's `wasmInput`. An initialized module or async module loader must provide the
matching SDK binding. `name`, `baseURL` and `consumerLinger` remain public options. `host.root` issues
managed contexts, `host.cancel` requests cancellation, and `host.connect` exposes connection readiness,
closure and status. Applications release references and unsubscribe their listeners; `host.close`
joins managed work and disposes tracked WASM wrappers. Exposing raw handles with a separate `free()`
would introduce another ownership boundary alongside that managed shutdown.

This consolidation also narrows the former raw consumer surface. It does not preserve every
previously exported ABI operation:

- Raw consumer revision snapshots through `WasmReadable.snapshotJson()` no longer have a public
  consumer equivalent. Producer `ManagedReadable.snapshot()` and internal Rust snapshots remain.
- Per-readable `awaitHydrated()` was removed from the WASM wrapper. Reference readiness and
  `get(..., { state: true })` remain, but are not identical to weakly waiting on one existing source.
- Raw `subscribeEvents()` was removed from the WASM wrapper. Consumers observe values rather than
  operation deltas or snapshot/reset lineage. The Rust core retains these event semantics internally.

The high-level TypeScript consumer `Readable<T>` already exposed `value` and `subscribe`; it still
does. The removed operations belonged to the separately exported raw WASM ABI. Likewise, the old
consumer-only module constructor and manual executor/candidate assembly are not a supported public
binary extension contract. Current module injection requires the matching full binding.

Module injection controls initialization, not Host adoption: `createHost` always creates and owns
a new full Rust Host. Attaching the facade to an externally owned `WasmHost`, or providing only the
old consumer ABI, is not supported. A future need for external Host adoption would require an
explicit ownership and shutdown contract, not a raw-handle getter.

Applications can already implement custom acquisition or evaluation and pass the resulting
`FacetDefinition` to `install()` or `reload([...])`. No raw prepared-candidate builder is needed.
The embedding owns those external evaluations and their retention policy; SDK loader budgets apply
to evaluations managed by `load`/`reload` loaders, not code already evaluated before registration.

There is no general `@tembra/sdk/browser/advanced` entry point. If a concrete embedding needs state
revision, hydration or event observations, evaluate a typed capability shared by Node and browser
over the same issued sources and owners. It must preserve Rust termination, event ordering and
weak ownership, without exposing pointers, raw JSON handles or executor disposal. Such an addition
requires a demonstrated use case and acceptance tests; it is not an implemented or promised API.

## Modules and responsibilities

| Module | Owns |
| --- | --- |
| `crates/core` (`tembra-core`) | Object identity, claims, contexts, state, resources, facet declarations, registries, execution versions, reload and lifecycle. |
| `crates/contracts` (`tembra-contracts`) | Portable contract descriptors, schema/identity rules and native adapter contracts. |
| `crates/hooks` (`tembra-hooks`) | Managed hook strategies and composition over core operations. |
| `crates/tembra` and `crates/macros` | Typed Rust application facade and generated contract/provider/reference API. |
| `crates/native` (`tembra-native`) | Native scheduling and transport adapters. |
| `crates/node-binding` | Node-API handles, callback ownership, generation admission and adapters to the Rust Host. |
| `crates/browser-wasm` | WASM binding, browser driver and checked JavaScript handles over the Rust Host. |
| `crates/executor-js` | Optional embedded JavaScript execution for Rust applications. |
| `crates/codegen` | Rust binding generation from verified portable descriptors. |
| `packages/sdk` | Shared TypeScript contracts and authoring API, platform facades/loaders, HostManagers and tooling entry points. |
| `testing` | Cross-language fixtures, integration suites and test support kept outside production libraries. |
| `examples` | Small applications using the final public API. |

`@tembra/sdk/tooling` provides programmatic typechecking, contract generation, bundling and
development delivery. These functions belong to build/development tooling; ordinary runtime imports use
`@tembra/sdk`, `@tembra/sdk/node` or `@tembra/sdk/browser`. There is no separate tooling package that
re-exports the runtime SDK.

## Ownership and state

The Host's address space uses contract name, normalized input and principal. An acquired reference
owns a claim; a binding wrapper carries that authority without creating another object model.
State cells, derivations and resources remain core-owned. JavaScript values may be retained by
issued resource handles, and generic binding adapters support portable hooks and registered opaque
JavaScript capabilities. Arbitrary Rust native types still need appropriate Rust adapters.

A call root pins its execution version. During compatible reload, new roots can use the replacement
while admitted old roots continue under their original version. The core handles state validation,
migration, resource handoff and retirement. Bindings retain the callbacks and values needed by
physical invocations until they settle, even if their public awaiter has already completed or left.

This boundary is the reason loader choice can change without replacing object identity, hooks or
reload semantics. A loader cannot independently free callbacks still owned by a Rust generation.

## Environment supervision

Code retirement in Tembra is not physical unloading of a JavaScript module. Native ESM caches and
other runtime references can retain code after Tembra releases its owners. Lower observed retention
with Jiti does not establish a universal collection guarantee. Cumulative finite admission therefore
applies to managed evaluation attempts and source costs; the [specification](spec.md#module-admission-and-memory)
records limits and diagnostics. These are admission bounds, not a cap on arbitrary trusted code's
heap allocations.

HostManagers separate environment supervision from object reload. Node uses `forkDriver` for child
processes; browser uses `webWorkerDriver` for Workers. Each environment has its own Rust Host.
Portable core RPC frames connect it to the supervising Host through a bounded transport pump;
opaque local values do not cross that connection.

An instance's `status()` and `closed` reflect observed platform/core lifecycle. `failure()` retains
the first causal terminal error; a normal stop leaves it undefined, while a failed stop can record
an error. `closed` marks completion of the tracked teardown attempt, not proof that every resource
closer succeeded. Inspect the failure and any rejected stop operation when assessing cleanup.

Connection liveness belongs to Rust RPC, with default ping interval 10 seconds and liveness deadline
30 seconds without peer traffic. Detection depends on scheduler progress; the manager does not run
an independent heartbeat. `activity()` includes transport pings and control frames, so increasing
traffic counters do not establish application demand or business health. Runtime suspension or a
blocked event loop can cause a connection timeout after scheduling resumes.

Started environments remain retained even with no application references. There is no automatic
idle disposal or Node `unref()` policy. Ownership ends through `stop()`, `replace()`, parent close
or failure-triggered teardown. Applications choose when their environment is no longer needed;
consumer linger policy does not control the process or Worker lifetime.

`start()` creates the child environment, `stop()` joins its managed cleanup before termination,
and `replace()` creates a fresh environment after stopping the previous one. Parent shutdown stops
owned children before closing parent services, so child resource closers can still use those services.
A forced termination or cleanup timeout cannot promise completed external cleanup.

Replacing an environment releases its runtime as an environment boundary, but loses that child's
in-memory objects, references and state. It is not an implementation of `host.reload()`. Persistence,
reconnection and migration across environment replacement are embedding-application concerns;
no loader silently restarts a Host to replenish its budget.

## Build and verification boundaries

The SDK has two build scripts, each with one output responsibility:

- `scripts/build-sdk.ts` emits the TypeScript SDK and declarations, preserving staged native/WASM assets.
- `scripts/build-bindings.ts` runs Cargo for Node-API or browser WASM and stages the generated assets.

These paths are relative to `packages/sdk`. The binding script runs directly with Node's TypeScript
support. Its Cargo metadata determines the target output directory, and it rejects a Node dependency
graph containing an embedded JavaScript engine. SDK emission and binding compilation have distinct
outputs; runtime facet loading invokes neither script. Applications call build/development APIs
directly with Bun; the SDK does not publish a command-line executable.

Unit tests remain with each Rust/TypeScript library. Cross-language and non-unit coverage is under
`testing/rust`, `testing/typescript` and shared `testing/fixtures`. Actual Node and browser integrations
exercise the bindings, compatible reload, retained references, hooks, opaque identity and cleanup.
The browser runner launches real Chromium; a TypeScript compile alone does not establish browser
runtime compatibility. Examples remain applications rather than test fixture emitters or low-level
adapter demonstrations.

## Implementation-owned Rust hook preparation

Rust consumers declare hook definitions eagerly through `HookManager::declare_series`,
`declare_bail`, `declare_waterfall`, `declare_middleware` and `declare_parallel` in their managed
implementation factory. The core validates the source Host and exact declaration pass before
running the producer without runtime borrows, then rechecks admission before attaching its value
to the returned implementation's existing owners. Failure does not adopt that producer's returned metadata; successful nested declarations and external producer/destructor effects
are not rolled back.

Strategy-typed handles contain only weak access to that exact owner. Each polled run validates
its current Context, execution version, receiver lifecycle and exact installed implementation.
Unchanged admitted implementation aliases may share metadata; replacement prepares new metadata.
Labels identify diagnostics only. No reactive State, global type map, registry plan, permission,
validated value or operation is cached. Runs delegate to the existing strategy pipelines and retain
their cancellation and physical-settlement ownership rules.

Checked Contracts retain exact canonical method-position keys beside their existing prepared
schemas. Hook types and descriptors share immutable metadata. The current selected operation's
Contract supplies profile comparison; equality data grants no execution authority. Canonical
preparation failures retain the previous deferred fallback behavior. Dynamic callers still select
their actual operation from the current Context's registry on every call. This Rust declaration
facility does not introduce a separate runtime or change foreign hook registration protocols.
