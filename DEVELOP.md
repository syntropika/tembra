# Development

This guide covers the development environment, build tools and runtime contracts.
For module layout and code style, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Tools

Use the pinned tool versions:

| Tool                           | Configuration                                          | Purpose                                    |
| ------------------------------ | ------------------------------------------------------ | ------------------------------------------ |
| Rust                           | [rust-toolchain.toml](rust-toolchain.toml)             | Cargo, rustfmt, Clippy and the WASM target |
| Bun                            | [packages/ts/.bun-version](packages/ts/.bun-version)   | TypeScript dependencies, builds and tests  |
| Node.js                        | [packages/ts/.node-version](packages/ts/.node-version) | Engine build scripts and Node acceptance   |
| Just                           | [CI workflow](.github/workflows/quality.yml)           | Repository commands                        |
| wasm-bindgen and cargo-machete | [justfile](justfile)                                   | WASM bindings and dependency checks        |
| Chrome or Chromium             | `TEMBRA_BROWSER`                                       | Browser and module Worker acceptance       |

Rustup reads the pinned Rust toolchain and installs its configured components and target.
Install Just, then install the build tools and TypeScript dependencies:

```sh
cargo install just --version 1.21.0 --locked
just install-tools
just install
```

Run these commands from the repository root. TypeScript tools and their lockfile are scoped to
`packages/ts`; the root Just commands select that workspace for you.

## Build

Build the engines before the SDK so generated WASM declarations are available:

```sh
just build-node
just build-wasm
just build-sdk
```

Each package also owns its build command:

```sh
bun run --cwd packages/ts/engine-node build --features=hooks,subscriptions,views,telemetry
bun run --cwd packages/ts/engine-wasm build --features=hooks,subscriptions,views
bun run --cwd packages/ts/sdk build
```

Engine package builds default to release mode. Pass `--debug` for a development build.
`just build-node` selects debug mode; `just build-wasm` selects release mode. Cargo compiles
Rust. The engine scripts package its output, and wasm-bindgen generates the browser loader.

The SDK build emits JavaScript, declarations and the bundled embedded adapter. Bun can import
SDK TypeScript sources directly. Engine artifacts live in their owning package's `dist` directory.

## Rust example

Run the native counter from the repository root:

```sh
cargo run --locked -p tembra --example counter
```

[`examples/rust/orb.rs`](examples/rust/orb.rs) declares the service, state and validation.
[`examples/rust/host.rs`](examples/rust/host.rs) installs the orb, calls its methods and awaits
host cleanup. The example prints `Count: 42`.

## Checks

```sh
just check
bun run --cwd packages/ts test:browser
```

`just check` builds the engines and SDK, then runs formatting, strict TypeScript, lint, Clippy,
dependency checks, Rust tests, SDK tests, Node acceptance and the WASM target check.
Browser acceptance runs separately against the built WASM engine.

| Command                                  | Purpose                                         |
| ---------------------------------------- | ----------------------------------------------- |
| `just fmt`                               | Format Rust, TypeScript and project documents   |
| `just fmt-check`                         | Check formatting without changing files         |
| `just lint`                              | Run Clippy and oxlint                           |
| `just deps`                              | Check for unused Rust dependencies              |
| `just test`                              | Build packages and run Rust, SDK and Node tests |
| `just check-wasm`                        | Check the WASM target with all features         |
| `bun run --cwd packages/ts typecheck`    | Check TypeScript across the workspace           |
| `bun run --cwd packages/ts test:browser` | Exercise Chrome and a module Worker             |

The browser runner uses `google-chrome` by default. Select another compatible executable with
`TEMBRA_BROWSER`, for example:

```sh
TEMBRA_BROWSER=chromium bun run --cwd packages/ts test:browser
```

Native acceptance uses the actual Node addon. Embedded acceptance uses V8 with a native Rust
provider through the same kernel. Build the SDK before running embedded tests directly.

Compare the same workloads when changing hot paths. Record the build profile, enabled features,
runtime and workload with every size or performance result. Keep profiling reports outside the
source tree.

## Compatibility

The API uses orb-owned validation. Applications using the schema-based API must migrate their
orb declarations; generated contract adapters are not part of this API.

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

## An orb

```ts
import { orb } from "@tembra/sdk";

export default orb({
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

Orbs can also supply an error descriptor with a built-in `Code`, their own reason and a message.
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
scope. Resources belong to the host, orb or object where they are declared. Dependencies remain
available while their consumer closes.

`host.reload([orb])` prepares and commits a replacement. `host.prepare([orb])` exposes an explicit
`commit()` / `abort()` handle for infrastructure coordination. Compatible code keeps already-admitted
roots on their old implementation. New roots use the adopted implementation. A state version
increase requires the owning orb's `migrate(previous, previousVersion)` callback. Incompatible
changes wait for complete roots that already touched the affected object or orb, including their
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

Hooks and their contributions are declared together with the orb and its `requires`. The engine
plans participants; orb callbacks define their behavior. SDK helpers include series, waterfall,
bail, parallel and middleware. Sources use `self.on(name, source, callback, options)`. Rust owns the
bounded delivery queue, reuse decisions and delivery scheduling; JS executes each requested callback.

Rust defaults omit optional capabilities. Enable `hooks`, `subscriptions` and `views` separately;
`telemetry` is independently optional.

RPC uses a host-selected codec over a byte transport. JSON is the default, with explicit BigInt and
byte-array representations. The optional MessagePack entrypoint preserves arbitrary-size BigInts.
Custom codecs must agree at both ends. Authentication, routing, persistence, databases and durable
coordination belong to infrastructure or applications. The optional coordinator is an in-memory
prepare/vote/commit helper; it provides no durable distributed transaction guarantee.

## Build orbs

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
owns its build and generated artifacts. A browser engine build produces a WASM module,
its generated JavaScript loader and types. Applications must serve the WASM asset alongside the
loader or supply its location through `wasmInput`.

The root `package.json` exposes the SDK for Git dependencies. Development manifests, the lockfile
and TypeScript tools live under `packages/ts`. Git consumers resolve the exact engine package
versions declared in that manifest. For local development, workspace links connect the SDK to the
engines built in this checkout. Engine builds run through their package commands.

Targets are `browser`, `worker`, `node` and `embedded`. Browser builds select browser package
conditions. Node builtins and explicitly declared `nativeExternals` require a Node host. Embedded
builds expose neither Node nor DOM APIs; provide environment capabilities through declared services
and resources. Runtime dependency discovery and unresolved dynamic module names are rejected.
Embedded artifacts use an IIFE to avoid retaining replaced modules in V8's ESM module map.
