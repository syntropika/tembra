# Tembra

Tembra is an in-memory runtime for versioned facets: services with immutable state, explicit
references, resources, tasks and declared dependencies. A facet owns its validation and business
logic. Tembra owns admission, revisions, lifetime and replacement.

This branch replaces the previous schema-based API. Existing applications must migrate their
facet declarations; the legacy API and generated contract adapters are removed.

## One engine

`tembra-core` implements the shared lifecycle in Rust. It decides when to open and close resources,
wait for physical work, gate a migration, adopt a replacement, recover an exclusive resource and
release retired scopes. Platform adapters receive execution requests and acknowledge callback
settlement. They do not implement a second lifecycle state machine.

JavaScript values and callbacks stay in their JavaScript realm. The engine receives opaque handles
and metadata. Native Rust calls use native values. Serialization applies when values actually cross
a language or transport boundary.

| Component                 | Responsibility                                                               |
| ------------------------- | ---------------------------------------------------------------------------- |
| `crates/core`             | Language-neutral ownership and lifecycle engine; no third-party dependencies |
| `crates/bridge`           | Metadata command and execution-request envelopes                             |
| `crates/tembra`           | Typed Rust declarations, values and native callback execution                |
| `crates/browser-wasm`     | The engine compiled to WASM; no interpreter, filesystem or DOM               |
| `crates/node-binding`     | Node-API access to the same engine                                           |
| `crates/executor-js`      | One embedded V8 runtime per Rust host                                        |
| `packages/ts/sdk`         | JS declarations, callback/value handles and optional infrastructure          |
| `packages/ts/engine-node` | Native engine package and its Cargo build                                    |
| `packages/ts/engine-wasm` | Browser engine package and its Cargo/wasm-bindgen build                      |

Node and browser hosts execute JavaScript in their existing realm. Embedded Rust hosts use V8.
A WASM build does not run native Rust plugins or include a JavaScript interpreter.

## A facet

```ts
import { facet } from "@tembra/sdk";

export default facet({
  name: "counter",
  apiVersion: 1,
  services: {
    counter: {
      create(self) {
        return {
          state: self.state(
            { count: 0 },
            {
              version: 1,
              validate(value) {
                if (!Number.isInteger(value.count) || value.count < 0)
                  throw new Error("Count must be a nonnegative integer");
              },
            },
          ),
        };
      },
      methods: {
        increment({ state }, ctx, amount: number) {
          return state.update(ctx, (current) => ({ count: current.count + amount })).count;
        },
      },
    },
  },
});
```

```ts
import { createHost } from "@tembra/sdk/node";
import counter from "./counter.js";

const host = createHost();
await host.install(counter);
const reference = await host.get("counter", ["main"]);
await reference.increment(1);
console.log(reference.state.value.count);
reference.release();
await host.close();
```

The browser entrypoint exports an asynchronous `createHost()` that loads the WASM kernel. It also
works in a module Worker. Applications can inject a kernel, module loader, host clock or codec.

Factories declare synchronously. Resource opening and tasks run after declaration. Local state
snapshots are recursively frozen plain data; repeated reads preserve identity. Selectors compare
with `Object.is`. Updates prepare and validate a candidate, check the invocation and revision,
adopt atomically, then notify observers. A rejected candidate leaves current state unchanged.

Use the exported `Lifetime`, `Status`, `ReloadStatus`, `Code`, `Reason` and `HookCapability` objects
for built-in names, for example `lifetime: Lifetime.Host`. Adapter commands and execution requests
also use closed named vocabularies; their serialized string values remain unchanged.

JavaScript failures carry the same `code`, `reason` and `message` fields as Rust errors. Built-in
descriptors keep their default messages together with their identities:

```ts
import { Errors, TembraError } from "@tembra/sdk";

throw new TembraError(Errors.Invalid.StateValidation);
```

Facets can also supply an error descriptor with a built-in `Code`, their own reason and a message.
Service names, object keys, principals and custom hook capabilities remain application-defined.
These named values require no schema, code generation or service argument/result declarations.

## Context metadata

Typed keys identify text, safe signed integers, booleans or owner-issued opaque references.
Rust validates key identity, version and scalar kind without generated validators:

```ts
import { ContextKey } from "@tembra/sdk";

const Trace = ContextKey.text("request/trace", 1);
await host.root(async (ctx) => {
  const tagged = ctx.with(Trace, "request-42");
  await reference.read(tagged);
  console.log(tagged.value(Trace));
});
```

`with()` preserves principal, implementation version and invocation lifetime. Calls and hook
contributions inherit their caller's frame. Concurrent branches remain isolated. Reusing a frame
passes its identity with the existing admission request; it does not copy all values or add a
propagation request. Reads walk at most 64 ancestors.

Frames remain indexed by their root until it ends and every physical call settles. Cancellation
does not release active work. Limits are 64 ancestors, 4096 frames per root, 1024 key contracts per
host, 256 bytes per key name and 4096 bytes per text value. Reuse derivations within long operations.

Storage owns opaque reference issuance, validation and revocation. Tembra retains the owner's
lease and drops it outside kernel borrows after physical settlement. Closing a storage scope can
revoke it earlier; an immutable context does not keep that scope open. `exportValues([Key])`
exports only explicitly selected scalars. Opaque references remain local.

## Ownership and replacement

Object identity is the service namespace, a tuple of string keys and a trusted principal. An
explicitly acquired reference is released explicitly; context-scoped dependencies belong to their
scope. Resources belong to the host, facet or object where they are declared. Dependencies remain
available while their consumer closes.

`host.reload([facet])` prepares and commits a replacement. `host.prepare([facet])` exposes an explicit
`commit()` / `abort()` handle for infrastructure coordination. Compatible code keeps already-admitted
roots on their old implementation. New roots use the adopted implementation. A state version
increase requires the owning facet's `migrate(previous, previousVersion)` callback. Incompatible
changes wait for complete roots that already touched the affected object or facet, including their
reentrant calls, and for owned tasks to settle. Other roots cannot enter those scopes during the
gate. Commit rejects older contexts for the upgraded scopes; compatible scopes keep admitting them.
A missed deadline aborts the reload. Start reloads outside a root that uses an affected scope:
awaiting a reload inside that root would make it wait for itself. Stale state writes are rejected.

An unchanged resource name, version, exclusivity and dependency list preserves its handle. An
adopted declaration supplies the callbacks for that retained resource; preparation and abort leave
its current callbacks intact. Increase the resource version when its physical value or cleanup
contract changes. An exclusive replacement closes the previous resource before opening its successor.
Failed preparation attempts to restore the previous resource; external effects are not a durable
transaction.

Cancellation is cooperative. Abandoning an observer or aborting its signal does not mean a callback
has stopped. Tasks, calls and resource openers retain ownership until they physically settle. Host
shutdown waits for that work; it cannot forcibly terminate an uncooperative callback.

## Hooks, sources and infrastructure

Hooks and their contributions are declared together with the facet and its `requires`. The engine
plans participants; facet callbacks define their behavior. SDK helpers include series, waterfall,
bail, parallel and middleware. Sources use `self.on(name, source, callback, options)`. Rust owns the
bounded delivery queue, reuse decisions and delivery scheduling; JS executes each requested callback.

Rust defaults omit optional capabilities. Enable `hooks`, `subscriptions` and `views` separately;
`telemetry` is independently optional.

RPC uses a host-selected codec over a byte transport. JSON is the default, with explicit BigInt and
byte-array representations. The optional MessagePack entrypoint preserves arbitrary-size BigInts.
Custom codecs must agree at both ends. Authentication, routing, persistence, databases and durable
coordination belong to infrastructure or applications. The optional coordinator is an in-memory
prepare/vote/commit helper; it provides no durable distributed transaction guarantee.

## Build facets

The build tool runs on Bun and emits one self-contained artifact per target:

```ts
import { build, Platform } from "@tembra/sdk/tooling";

await build({
  builds: {
    web: { entry: "src/counter.ts", platform: Platform.Browser },
    server: { entry: "src/counter.ts", platform: Platform.Embedded },
  },
});
```

Install `@tembra/sdk` once. Build tooling is available through `@tembra/sdk/tooling`.

Bun resolves the SDK directly from TypeScript sources. Browser bundlers can use the `browser`
export condition; other bundlers can select `source`. Node's default ESM entrypoints use the
compiled JavaScript output. These entrypoints share the same implementation.

The SDK depends on separate `@tembra/engine-node` and `@tembra/engine-wasm` packages. Each engine
owns its build and generated artifacts. The browser package contains a precompiled WASM module,
its generated JavaScript loader and types. Applications must serve the WASM asset alongside the
loader or supply its location through `wasmInput`.

The root `package.json` is an installation facade for Git dependencies. Development manifests,
the lockfile and TypeScript tools live under `packages/ts`. Installing the SDK from Git requires
the exact engine versions declared in that facade to be published separately. No installation
hook compiles Rust or downloads unpublished binaries.

Targets are `browser`, `worker`, `node` and `embedded`. Browser builds select browser package
conditions. Node builtins and explicitly declared `nativeExternals` require a Node host. Embedded
builds expose neither Node nor DOM APIs; provide environment capabilities through declared services
and resources. Runtime dependency discovery and unresolved dynamic module names are rejected.
Embedded artifacts use an IIFE to avoid retaining replaced modules in V8's ESM module map.

## Development

Use the versions pinned in the repository tool configuration, including Bun, Rust and wasm-bindgen.

```sh
just install
just build-node
just build-wasm
just build-sdk
just test
bun run --cwd packages/ts test:browser
just check
```

Each package also has its own build command:

```sh
bun run --cwd packages/ts/engine-node build --features=hooks,subscriptions,views,telemetry
bun run --cwd packages/ts/engine-wasm build --features=hooks,subscriptions,views
bun run --cwd packages/ts/sdk build
```

Engine builds default to release mode. Pass `--debug` for a development build. The SDK build emits
JavaScript, declarations and the bundled embedded adapter; importing its TypeScript sources on Bun
does not require that build. Building a native or WASM engine remains a separate development step.

Native integration tests use the actual Node addon. Browser acceptance launches Chrome to exercise
both the browser and module Worker. Embedded integration uses V8 with a native Rust provider through
the same kernel. Build the SDK before running the embedded tests directly.

Size and performance results must identify the build profile, enabled features, runtime and workload.
A partial engine build is not a complete replacement benchmark.
