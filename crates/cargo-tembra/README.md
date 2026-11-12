# cargo-tembra

A Cargo subcommand for Rust Orbs, in the spirit of `cargo contract` and the CosmWasm optimizer.
An author writes a module with [`#[orb]`](../orb-macro/README.md) and runs one command; the WIT,
the component checks, the artifact and the grant summary follow.

```sh
cargo install --git https://github.com/syntropika/tembra cargo-tembra --locked
# or, in a checkout of the repository:
cargo install --path crates/cargo-tembra
cargo tembra --help
```

## Commands

| Command                                                              | Effect                                                                                                                                                                                                                                                                                                     |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cargo tembra new <name> --package ns:name@1.0.0 [--tembra <root>]`  | creates an Orb crate: `Cargo.toml` with `[package.metadata.tembra]`, `src/lib.rs` with an `#[orb]` module and a first method; the crate depends on the `tembra` facade by path in a checkout or with `--tembra`, elsewhere through git at the commit `cargo-tembra` was built from                         |
| `cargo tembra build [--debug] [--out <dir>]`                         | compiles for `wasm32-unknown-unknown` with `wasm-component-ld`, writes the contract to `contract/`, verifies the component against it, packages into `artifacts/`, writes checksums and summarizes services with their lifetime, hooks declared and contributed to, kernel state and resources, and grants |
| `cargo tembra wit <component.wasm> [--emit <dir> --world <name>]`    | prints the WIT reconstructed from a component, or writes its contract as a `.wit` file                                                                                                                                                                                                                     |
| `cargo tembra inspect <component.wasm> [--wit <dir> --world <name>]` | core import sections, services and methods, required grants and, against a WIT, hook contributions and the verifier's verdict                                                                                                                                                                              |

The crate `new` creates depends on the public `tembra` facade with only its `macros` feature. In a
tembra checkout, found by walking up from the current directory, or in the one `--tembra <root>`
names, it takes the facade from the checkout's `crates/tembra`, by path. Anywhere else it takes it
from this repository through git, at the commit this `cargo-tembra` was built from, so that the
macro the crate compiles with writes the contract and manifest this `cargo-tembra` reads; after
installing `cargo-tembra` from another commit, move the crate's `rev` along.

The build of `cargo-tembra` reads that commit with git from the sources it compiles: the checkout
`cargo install --git` builds in keeps its `.git`, as a clone or a worktree does for
`cargo install --path`. Built without git, or from sources without `.git` such as an archive,
`cargo-tembra` knows no commit, and `new` outside a checkout asks for `--tembra` rather than
depend on whatever the default branch holds. A commit the repository does not have, one not
pushed yet, fails when Cargo fetches it; `--tembra` serves there too.

## Where the contract comes from

A component built with the macro carries the module's WIT in a `tembra:wit` custom section and
its manifest in a `tembra:manifest` section: the hooks it declares, with the composition from
`#[hook(..., composition = c)]`, the hooks it contributes to, with the priority from
`#[hook(..., priority = n)]`, the services that chose a lifetime or a linger with
`#[service(lifetime = host)]` or `#[service(linger_ms = n)]`, the view of each interface it imports
with `import("ns:name/interface@version", keyed)` and the interface the view reaches, and the
directory of the WIT packages the module consumes, as `wit = "dir"` wrote it. `build` takes the contract from there, so it is
the text the macro derived: `///` comments included, and with the hook imports the linker drops
because nothing calls them. The packages the Orb consumes, a hook's interface, an interface it
implements or imports, are nested after it whole, read from that directory, so an imported
interface keeps the functions the component never calls and names the same interface its provider
exports; a component without that directory gets them from its own types. A package a nested one
builds on is nested too, another Orb's import for instance, though the component never reaches
it. The standard packages that ship with `tembra-contracts`, WASI and the kernel's `tembra:guest`,
are not nested: each one the contract depends on, directly or through a nested package, is written
to `contract/deps/` as `tembra-contracts` bundles it. Any other component gets a contract derived
from its types alone, with the world name from the metadata and every interface the world both
imports and exports a contribution at priority 0.

`contract/` thus resolves on its own, in the layout `wit-parser` reads: another Orb consumes it
with `wit = "../x/contract"`, and `Contract::from_dir` reads it, as `inspect --wit` does, skipping
the bundled copies in `deps/`. One limit remains: an Orb that keeps state, owns resources or reads
the context does not compile yet against a contract holding `deps/tembra-guest.wit`, because the
macro adds the kernel's package to its bindings a second time.

The contract goes to `contract/`, not `wit/`: `wit-bindgen` loads a `wit` directory inside the
crate on its own, even with an inline WIT, and finding the package twice it panics with
"attempting to re-add package".

`build` reads the contract on its own before writing it, so no other file in `contract/` takes
part in it, and checks that it resolves with its standard packages alone. The world is named after
the Orb's module: when the module is renamed, the `.wit` file of the former name, which declares
the same package, is removed, and `build` says so on stderr. A standard package in `deps/` the
contract no longer depends on is removed the same way. Any other file stays.

## Manifest metadata

```toml
[package.metadata.tembra]
name = "filter"         # the Orb name; default: the crate name
api-version = 1         # default 1
world = "filter"        # only for a component without an embedded contract; default: the Orb name
```

The name and the API version live here because the WIT does not carry them: they are two of the
arguments `Contract::component_artifact` takes; another is its `ComponentRoles`. Hook interfaces are
read from the contract, the interfaces its world both imports and exports; the component says
which it declares and which it contributes to, with composition and priority, which of its
services chose a lifetime, and which of its imports are views of interfaces it imports by key,
with the interface each reaches. `build` reads these roles with `Contract::embedded_roles` of
`tembra-contracts`, as the SDK's `build()` does for a `.wasm` entry, so the component carries the
same roles whichever tool packages it.

## Build outputs

```text
<crate>/contract/
  <world>.wit        the contract: the Orb's WIT, consumed packages nested after it; commit it
  deps/              the standard packages it depends on, such as tembra-guest.wit; commit them
<crate>/artifacts/
  <name>.wasm        the compiled component
  <name>.orb.wasm    the Tembra artifact: the component plus the tembra:orb section
  checksums.txt      SHA-256 of both files
```

Both directories sit inside the Orb's crate, like `schema/` and `artifacts/` of a CosmWasm contract:
the first is committed, the second is a build output to ignore. With `--out <dir>` the component,
the artifact, a copy of the contract and the checksums go to that directory instead, so a
repository with several Orbs can collect them in one place. A build replaces the lines of its own
files in `checksums.txt` and keeps the others, sorted by file name.

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
An interface imported by key is listed among the imports of other Orbs by its view, the binding
`Install::requires` names, followed by `by key, the objects of` and the interface it reaches.

For an Orb that imports WASI, the last part names the grants an installer must give, for example
`system: cli` and `system: clocks` for a std Orb that prints. Without it, those fourteen `wasi:cli`
and `wasi:clocks` interfaces surface one at a time from `engine.load` errors. A `wasi:clocks`,
`wasi:random` or `wasi:cli` interface that installation does not bind, such as
`wasi:clocks/timezone` or one of another version than `0.3.0`, is listed as
`NOT SUPPORTED by Tembra`.

A component exporting `wasi:http/handler@0.3.0` gets a line of its own for this incoming HTTP
handler, which the host's ingress calls and which is never a service. Unless the component also
imports `wasi:http/client`, whose HTTP grant then covers them, its `wasi:http/types` need no grant.

## Browsers

The artifact runs in browsers through an adapter the application's build prepares from these
exact bytes with `prepareBrowserComponent` of `@syntropika/tembra-sdk/tooling`; see
[browser preparation](../../docs/tooling.md#prepare-a-compiled-orb-for-browsers). The adapter
serves the kernel interfaces and the declared hooks itself, as the native executor does.

## Not yet

- Registry-based scaffolding instead of a checkout path or a git commit. The facade already
  exports the macro; generated bindings still use the explicit `wit-bindgen` and `dlmalloc`
  dependencies.
- `run --call`, behind an optional feature, to install and call the Orb with the executor.
- `wasm-opt`, once Binaryen handles components; today it works on core modules only.
- Reproducible builds in a container, like the optimizer, when a marketplace must trust a hash.
