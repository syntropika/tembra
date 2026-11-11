# cargo-tembra

A Cargo subcommand for Rust Orbs, in the spirit of `cargo contract` and the CosmWasm optimizer.
An author writes a module with [`#[orb]`](../orb-macro/README.md) and runs one command; the WIT,
the component checks, the artifact and the grant summary follow.

```sh
cargo install --path crates/cargo-tembra
cargo tembra --help
```

## Commands

| Command                                                              | Effect                                                                                                                                                                                                                                                                                                     |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cargo tembra new <name> --package ns:name@1.0.0`                    | creates an Orb crate: `Cargo.toml` with `[package.metadata.tembra]`, `src/lib.rs` with an `#[orb]` module and a first method                                                                                                                                                                               |
| `cargo tembra build [--debug] [--out <dir>]`                         | compiles for `wasm32-unknown-unknown` with `wasm-component-ld`, writes the contract to `contract/`, verifies the component against it, packages into `artifacts/`, writes checksums and summarizes services with their lifetime, hooks declared and contributed to, kernel state and resources, and grants |
| `cargo tembra wit <component.wasm> [--emit <dir> --world <name>]`    | prints the WIT reconstructed from a component, or writes its contract as a `.wit` file                                                                                                                                                                                                                     |
| `cargo tembra inspect <component.wasm> [--wit <dir> --world <name>]` | core import sections, services and methods, required grants and, against a WIT, hook contributions and the verifier's verdict                                                                                                                                                                              |

`new` finds the tembra checkout by walking up from the current directory, because the created
crate depends on the macro by path; outside the checkout, pass `--tembra <root>`.

## Where the contract comes from

A component built with the macro carries the module's WIT in a `tembra:wit` custom section and
its manifest in a `tembra:manifest` section: the hooks it declares, with the composition from
`#[hook(..., composition = c)]`, the hooks it contributes to, with the priority from
`#[hook(..., priority = n)]`, the services that chose a lifetime or a linger with
`#[service(lifetime = host)]` or `#[service(linger_ms = n)]`, and the directory of the WIT packages
the module consumes, as `wit = "dir"` wrote it. `build` takes the contract from there, so it is
the text the macro derived: `///` comments included, and with the hook imports the linker drops
because nothing calls them. The packages the Orb consumes, a hook's interface, an interface it
implements or imports, are nested after it whole, read from that directory, so an imported
interface keeps the functions the component never calls and names the same interface its provider
exports; a component without that directory gets them from its own types. The standard packages
that ship with `tembra-contracts`, WASI and the kernel's `tembra:guest`, are not nested. Any other
component gets a contract derived from its types alone, with the world name from the metadata and
every interface the world both imports and exports a contribution at priority 0.

The contract goes to `contract/`, not `wit/`: `wit-bindgen` loads a `wit` directory inside the
crate on its own, even with an inline WIT, and finding the package twice it panics with
"attempting to re-add package".

## Manifest metadata

```toml
[package.metadata.tembra]
name = "filter"         # the Orb name; default: the crate name
api-version = 1         # default 1
world = "filter"        # only for a component without an embedded contract; default: the Orb name
```

The name and the API version live here because the WIT does not carry them: they are two of the
arguments `Contract::component_artifact` takes; the third is the manifest. Hook interfaces are
read from the contract, the interfaces its world both imports and exports; the component says
which it declares and which it contributes to, with composition and priority, and which of its
services chose a lifetime.

## Build outputs

```text
<crate>/contract/
  <world>.wit        the contract: the Orb's WIT, consumed packages nested after it; commit it
<crate>/artifacts/
  <name>.wasm        the compiled component
  <name>.orb.wasm    the Tembra artifact: the component plus the tembra:orb section
  checksums.txt      SHA-256 of both files
```

Both directories sit inside the Orb's crate, like `schema/` and `artifacts/` of a CosmWasm contract:
the first is committed, the second is a build output to ignore. With `--out <dir>` the component,
the artifact, a copy of the contract and the checksums go to that directory instead, so a
repository with several Orbs can collect them in one place.

and a summary, here for the filter of the hooks example:

```text
exports, the services of filter:
  filter::example:filter/report@1.0.0  methods: read
hook contributions, bound to the provider with Install::requires:
  example:admission/guard@1.0.0  priority -7
other imports: none; the installer grants nothing
```

An Orb declaring a hook adds `hooks declared, run by the kernel's plan of contributions:` with the
composition of each. A service that chose a lifetime or a linger says so after its methods, for
example `lifetime: host, pinned until the host closes`. An Orb with a `#[state]`, a
`#[resource]` or `context` on the attribute adds a line naming the kernel's interface it uses:
the executor serves it, so it appears among neither the imports to bind nor the grants to give.

For an Orb that imports WASI, the last part names the grants an installer must give, for example
`system: cli` and `system: clocks` for a std Orb that prints. Without it, those fourteen `wasi:cli`
and `wasi:clocks` interfaces surface one at a time from `engine.load` errors.

## Not yet

- A `tembra-guest` crate re-exporting `wit-bindgen`, `dlmalloc` and the macro, so that `new` does
  not depend on a path inside this checkout, and publication on crates.io next to
  `tembra-contracts`.
- `run --call`, behind an optional feature, to install and call the Orb with the executor.
- `wasm-opt`, once Binaryen handles components; today it works on core modules only.
- Reproducible builds in a container, like the optimizer, when a marketplace must trust a hash.
