# Rust Orb exporting a service

This example writes a compiled Orb in Rust without a WIT file or a build script, builds it with
`cargo tembra` and installs the artifact through the ordinary Rust host.

| File                           | Role                                                                                  |
| ------------------------------ | ------------------------------------------------------------------------------------- |
| `greeter/src/lib.rs`           | The Orb: one module under `#[orb]`; the contract and the code in one place            |
| `greeter/contract/greeter.wit` | The WIT derived from the component by `cargo tembra build`, committed as the contract |
| `greeter/Cargo.toml`           | `[package.metadata.tembra]` names the Orb and its API version                         |
| `host/src/main.rs`             | The wiring: loads the packaged artifact and calls two methods; it defines no Orb      |
| `run.sh`                       | Build, show the derived WIT, run the host                                             |

`greeting` greets by name and measures a text. The author writes a `pub struct Stats`, a unit
struct `Greeting` and three `pub fn`; [the macro](../../crates/orb-macro/README.md) turns them into a
`record`, an exported `interface` and three `func`s, and generates the `wit-bindgen` bindings and
the `export!`.

## Build

Run from the repository root. The pinned toolchain already includes the `wasm32-unknown-unknown`
target.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-service/greeter/Cargo.toml
```

`build` compiles the crate on stable with the component linker, derives the WIT from the component
into `greeter/contract/greeter.wit`, checks the component against it and writes `greeter.wasm`,
`greeter.orb.wasm` and `checksums.txt` into `greeter/artifacts/`, next to the contract. The summary lists the services and the grants
an installer must give; this Orb imports nothing.

## Run

```sh
cargo run --manifest-path examples/rust-orb-service/host/Cargo.toml -- examples/rust-orb-service/greeter/artifacts/greeter.orb.wasm
```

The host prints `"Hello, Ada!"` and `{"chars":22,"words":5}`. It loads the artifact with
`Install::default()`: the world imports nothing, so there is nothing to grant. `run.sh` runs the
three steps in order.

## Reading the generated code

The macro's output is ordinary Rust. With the nightly named in the justfile and its
`wasm32-unknown-unknown` target:

```sh
cargo +"$(just --evaluate nightly)" rustc --manifest-path examples/rust-orb-service/greeter/Cargo.toml \
  --target wasm32-unknown-unknown -- -Zunpretty=expanded
```

## Limits

Imports of other Orbs and of WASI interfaces are not expressed by the macro yet; see
[the macro's limits](../../crates/orb-macro/README.md#limits). Contributing to another Orb's hook is
shown in [rust-orb-hook-contribution](../rust-orb-hook-contribution/README.md), declaring one in
[rust-orb-hook-declaration](../rust-orb-hook-declaration/README.md).
