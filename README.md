# Tembra

A Rust runtime for typed services, managed objects, reactive state and reloadable JavaScript,
native and in browser WebAssembly.

Tembra keeps object identity, lifetimes and state in one Rust core. Node-API and browser JavaScript
bindings, and the optional embedded V8 executor, build on the same runtime model.

## How it works

Declare a contract, register a provider and acquire a typed reference. The host resolves objects by
contract name, normalized input and principal, then manages their dependencies and resources.
References call the current implementation and expose its reactive state.

- **Typed contracts:** Rust macros and TypeScript declarations describe services; portable contracts
  validate values across language and remote boundaries.
- **Managed lifetimes:** references retain objects, dependencies and resources until cleanup.
- **Reactive state:** atomic local updates, immutable snapshots, derived values and revisioned deltas.
- **RPC:** shared connection logic with native and browser WebSocket adapters.
- **Facets and hooks:** compose implementations through versioned registries and explicit hook pipelines.
- **JavaScript reload:** compatible replacement retains object identity, references and state; admitted
  calls keep their execution version while new calls use the committed version.

The host supplies scheduling, transport and trusted capabilities through adapters. Compilation,
dependency resolution and file watching live in external tooling.

## Build from source

Install Git, Rustup, just 1.21 or later and Bun 1.4.3. The checked-in toolchain selects Rust 1.99.0,
rustfmt, Clippy and the WebAssembly target. Native builds need a C/C++ toolchain; the JavaScript
executor also builds or downloads its pinned V8 dependency.

```sh
git clone https://github.com/syntropika/tembra.git
cd tembra
just setup
just install-ts
just run-rust-example
```

The counter prints `Count: 1` and completes managed shutdown. Its
[entrypoint](examples/rust/src/main.rs) registers a provider, acquires a reference and increments
managed state. The [contract and provider](examples/rust/src/counter.rs) are exercised by the
[counter integration tests](testing/rust/tests/examples/counter.rs), including overflow refusal.

## Rust

Declare a local contract and implement its generated provider trait:

```rust
use tembra::prelude::*;

#[tembra::contract(local, name = "example.counter", version = 1)]
trait Counter {
    #[state]
    fn count(&self) -> u32;
    async fn increment(&self) -> Result<u32>;
}

struct CounterService {
    count: Cell<u32>,
}

impl CounterProvider for CounterService {
    fn count(&self) -> Readable<u32> {
        self.count.readable()
    }

    async fn increment(&self, ctx: Context) -> Result<u32> {
        self.count.change(&ctx, |count| {
            *count = count.checked_add(1).ok_or_else(|| {
                RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
            })?;
            Ok(())
        })?;
        self.count.value()
    }
}

fn main() -> Result<()> {
    tembra::native::run("example", (), |host| async move {
        host.serve(Counter, |_, _, declaration| {
            Ok(CounterService {
                count: declaration.state("count", 0_u32)?,
            })
        })?;
        let counter = host.get(Counter, CounterInput {}).await?;
        counter.increment().await?;
        println!("Count: {}", counter.count().value()?);
        drop(counter);
        Ok(())
    })
}
```

Enable the `native` feature for this runner. It awaits host shutdown after the callback finishes,
including on error. Local Rust method values stay native; portable remote calls use runtime
validation. At `u32::MAX`, this provider refuses the increment without changing state.

## Integrations

| Environment | Entry point | Source |
| --- | --- | --- |
| Rust | `tembra::prelude`, `tembra::native::run` | [Typed facade](crates/tembra) |
| Node.js | `@tembra/sdk/node` over the Rust Host through Node-API | [Node bindings](crates/node-binding) |
| Embedded JavaScript | Optional V8 executor for Rust applications | [Executor](crates/executor-js) |
| Browser | `@tembra/sdk/browser` over the Rust WASM core | [WASM bindings](crates/browser-wasm) |
| TypeScript contracts | `@tembra/sdk` | [SDK](packages/sdk) |
| Builds and development | `@tembra/sdk/tooling` programmatic API | [SDK tooling](packages/sdk/src/tooling) |

The TypeScript SDK provides declarations and bindings over the Rust core. Platform Host usage is
documented in the [guide](docs/guide.md#typescript-hosts).

## Development

Use the [justfile](justfile) for project commands:

```sh
just check-all
just test-doc
```

`check-all` runs Rust and TypeScript tests, formatting, strict lint, dependency checks, WebAssembly
compilation and the native counter. It does not launch a browser. `test-doc` executes Rust
documentation examples. TypeScript build and development tools are functions exported by
`@tembra/sdk/tooling`; applications own their configuration, cancellation and delivery adapters.

Native facets remain fixed for their Host lifetime. Hot reload targets JavaScript implementations.
State retained through a compatible reload is in-memory state; applications supply durable storage.
A disconnected remote call can have an unknown execution outcome and is not automatically retried.

## Documentation

- [Guide](docs/guide.md): contracts, objects, state and language integrations.
- [Tutorial](docs/tutorial.md): run and understand the Rust counter.
- [Runtime behavior](docs/spec.md): lifecycle, state, reload and remote-call guarantees.
- [Architecture](docs/architecture.md): platform loaders, bindings and the authoritative Rust core.

## Acknowledgements

Tembra draws on [Chord](https://github.com/earendil-works/pi/tree/e17e2e10556cea6b674bb627726bef8ceed90065/packages/chord)
for its managed object model, reactive state and reload semantics, and on
[Cordis](https://github.com/cordiverse/cordis) for its approach to composable services and plugin
lifecycles. Thanks to their authors and contributors. Tembra implements its own Rust runtime and
protocol; compatibility with either project is not implied.
