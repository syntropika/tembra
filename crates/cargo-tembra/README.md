# cargo-tembra

A Cargo subcommand for Rust Orbs, in the spirit of `cargo contract` and the CosmWasm optimizer.
An author writes a module with [`#[orb]`](../orb-macro/README.md) and runs one command; the WIT,
the component checks, the artifact and the grant summary follow.

```sh
cargo install --path crates/cargo-tembra
cargo tembra --help
```

## Commands

| Command                                                                     | Effect                                                                                                                                                                                                                                               |
| --------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cargo tembra new <name> --package ns:name@1.0.0`                           | creates an Orb crate: `Cargo.toml` with `[package.metadata.tembra]` and the dependencies the macro's code names, `src/lib.rs` with an `#[orb]` module and a first method                                                                             |
| `cargo tembra build [--debug] [--manifest-path <Cargo.toml>] [--out <dir>]` | compiles for `wasm32-unknown-unknown` with `wasm-component-ld`, writes the contract to `contract/`, verifies the component against it, packages into `artifacts/`, writes checksums and summarizes services, hooks, the kernel lifecycle and imports |
| `cargo tembra wit <component.wasm> [--emit <dir> --world <name>]`           | prints the WIT reconstructed from a component, or writes its contract as a `.wit` file                                                                                                                                                               |
| `cargo tembra inspect <component.wasm> [--wit <dir> --world <name>]`        | core import sections, services and methods, imports and grants and, against a WIT, the hooks told apart and the verdict of the check `build` runs                                                                                                    |

`new` finds the tembra checkout by walking up from the current directory, because the created
crate depends on the macro by path; outside the checkout, pass `--tembra <root>`.

## Where the contract comes from

A component built with the macro carries the module's WIT in a `tembra:wit` custom section and,
when the module consumes WIT packages, the directory holding them, as `wit = "dir"` wrote it, in a
`tembra:consumed` section. `build` takes the contract from there, so it is the text the macro
derived: `///` comments included, and with the imports of the hooks the Orb contributes to, which
the linker drops because nothing calls them. The packages the Orb consumes, a hook's interface, an
interface it implements or imports, are nested after it whole, read from that directory, so an
imported interface keeps the functions the component never calls and names the same interface its
provider exports; a component that names no directory gets them from its own types. The packages
that ship with `tembra-contracts`, WASI and the kernel's private `tembra:orb`, are never nested:
the contract does not mention `tembra:orb` at all. Any other component gets a contract derived from
its types alone, with the world name from the metadata; packaging it still needs the lifecycle the
macro generates, as the next section says.

The contract goes to `contract/`, not `wit/`: `wit-bindgen` loads a `wit` directory inside the
crate on its own, even with an inline WIT, and finding the package twice it panics with
"attempting to re-add package".

## Packaging

`build` packages the component with `Contract::source_component_artifact`, given the component,
the Orb's name and its API version: the call the TypeScript build makes for a componentized
JavaScript Orb. The check trusts no metadata, only the component's own types: its exports are the
contract's plus the kernel's `tembra:orb/lifecycle@0.1.0`, its private imports a subset of
`tembra:orb/host@0.1.0`, an import of one of its own exported interfaces, the entry to a hook it
declares, has that export's signatures, and every other import belongs to the contract. The
artifact is the component with two sections appended: `tembra:orb`, the Orb's identity and its
compact contract, and `tembra:runtime`, which names the private package the component was built
against.

No manifest travels with it. The services and their lifetime and linger, the hooks the Orb
declares, with their composition, and the hooks it contributes to, with their priority, are what
the component's `lifecycle.describe` returns; the host checks that description against the
contract before anything runs.

## Manifest metadata

```toml
[package.metadata.tembra]
name = "filter"         # the Orb name; default: the crate name
api-version = 1         # default 1
world = "filter"        # only for a component without an embedded contract; default: the Orb name
```

The name and the API version live here because the WIT does not carry them: they are the two
arguments `Contract::source_component_artifact` takes besides the component. The macro reads the
same two at compile time and reports them in `lifecycle.describe`, and the host refuses an
artifact registered under another name or API version than the one its component describes.

## Build outputs

```text
<crate>/contract/
  <world>.wit        the contract: the Orb's WIT, consumed packages nested after it; commit it
<crate>/artifacts/
  <name>.wasm        the compiled component
  <name>.orb.wasm    the Tembra artifact: the component plus the tembra:orb and tembra:runtime sections
  checksums.txt      SHA-256 of both files
```

Both directories sit inside the Orb's crate, like `schema/` and `artifacts/` of a CosmWasm contract:
the first is committed, the second is a build output to ignore. With `--out <dir>` the component,
the artifact, a copy of the contract and the checksums go to that directory instead, so a
repository with several Orbs can collect them in one place.

`build` ends with a summary, here for the filter of the hook contribution example:

```text
exports, the services of filter:
  filter::example:filter/report@1.0.0  methods: read
hook contributions, bound to the provider with Install::requires:
  example:admission/guard@1.0.0
kernel lifecycle: the host describes, creates and releases this Orb through tembra:orb/lifecycle@0.1.0 and answers its state, resources, hooks and context through tembra:orb/host@0.1.0
imports: none; the installer grants nothing
```

An Orb declaring a hook lists it under `hooks declared, run by the kernel's plan of contributions:`.
The summary tells a declaration from a contribution by the contract, because the linker drops the
import of a contribution the component never calls: an exported interface the contract imports too
is a contribution, one the component alone imports, to run it, is a hook the Orb declares.
`inspect` without `--wit` has no contract to ask, so it lists the hooks the component imports as
`hook interfaces, declared or contributed to`, and a contribution looks like a service there.

The `kernel lifecycle` line appears for every Orb the macro builds: the host links `tembra:orb`
itself, so the package appears among neither the imports to bind nor the grants to give. The
lifetime and the linger of the services are not printed; they travel in `lifecycle.describe`. The
last part lists the imports: the interfaces of other Orbs, which the installer binds with
`Install::requires`, and WASI interfaces grouped by grant. The macro expresses no WASI import, so
an Orb it builds imports other Orbs' interfaces or nothing.

## Not yet

- A `tembra-guest` crate re-exporting `wit-bindgen`, `dlmalloc` and the macro, so that `new` does
  not depend on a path inside this checkout, and publication on crates.io next to
  `tembra-contracts`.
- `run --call`, behind an optional feature, to install and call the Orb with the executor.
- `wasm-opt`, once Binaryen handles components; today it works on core modules only.
- Reproducible builds in a container, like the optimizer, when a marketplace must trust a hash.
