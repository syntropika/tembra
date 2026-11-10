# cargo-tembra

A Cargo subcommand for Rust Orbs, in the spirit of `cargo contract` and the CosmWasm optimizer.
An author writes a module with [`#[orb]`](../orb-macro/README.md) and runs one command; the WIT,
the component checks, the artifact and the grant summary follow.

```sh
cargo install --path crates/cargo-tembra
cargo tembra --help
```

## Commands

| Command                                                              | Effect                                                                                                                                                                                                         |
| -------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cargo tembra new <name> --package ns:name@1.0.0`                    | creates an Orb crate: `Cargo.toml` with `[package.metadata.tembra]`, `src/lib.rs` with an `#[orb]` module and a first method                                                                                   |
| `cargo tembra build [--debug] [--out <dir>]`                         | compiles for `wasm32-unknown-unknown` with `wasm-component-ld`, derives the WIT from the component into `contract/`, verifies, packages into `artifacts/`, writes checksums and lists the grants the Orb needs |
| `cargo tembra wit <component.wasm> [--emit <dir> --world <name>]`    | prints the WIT reconstructed from a component, or writes it as a `.wit` file                                                                                                                                   |
| `cargo tembra inspect <component.wasm> [--wit <dir> --world <name>]` | core import sections, services and methods, required grants and, against a WIT, the verifier's verdict                                                                                                         |

`new` finds the tembra checkout by walking up from the current directory, because the created
crate depends on the macro by path; outside the checkout, pass `--tembra <root>`.

## Manifest metadata

```toml
[package.metadata.tembra]
name = "greeter"        # the Orb name; default: the crate name
api-version = 1         # default 1
world = "greeter"       # default: the Orb name in kebab-case, which the macro derives from the module name
# contributes = { "example:hooks/guard@1.0.0" = -7 }   # hook contribution priorities
```

The name and the API version live here because the WIT does not carry them: they are the two
arguments `Contract::component_artifact` takes.

## Build outputs

```text
contract/
  <world>.wit        the WIT derived from the component: the contract to commit
artifacts/
  <name>.wasm        the compiled component
  <name>.orb.wasm    the Tembra artifact: the component plus the tembra:orb section
  checksums.txt      SHA-256 of both files
```

and a summary, here for the example greeter:

```text
exports, the services of greeter:
  greeter::example:greeter/greeting@1.0.0  methods: hello, count-words, measure
imports: none; the installer grants nothing
```

For an Orb that imports WASI, the last part names the grants an installer must give, for example
`system: cli` and `system: clocks` for a std Orb that prints. Without it, those fourteen `wasi:cli`
and `wasi:clocks` interfaces surface one at a time from `engine.load` errors.

The WIT goes to `contract/`, not `wit/`: `wit-bindgen` loads a `wit` directory inside the crate on
its own, even with an inline WIT, and finding the package twice it panics with "attempting to
re-add package".

## Not yet

- A `tembra-guest` crate re-exporting `wit-bindgen`, `dlmalloc` and the macro, so that `new` does
  not depend on a path inside this checkout, and publication on crates.io next to
  `tembra-contracts`.
- Reading the WIT from a `tembra:wit` custom section written by the macro, which would keep the
  `///` comments the binary format drops.
- `run --call`, behind an optional feature, to install and call the Orb with the executor.
- `wasm-opt`, once Binaryen handles components; today it works on core modules only.
- Hooks: `contributes` already reaches `component_artifact`; the macro side, an attribute
  generating the import, the export and the bridge, is missing.
- Reproducible builds in a container, like the optimizer, when a marketplace must trust a hash.
