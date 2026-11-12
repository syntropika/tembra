# Rust Orb implementing a shared WIT interface

This example writes a compiled Orb that implements an interface it did not write. The contract,
`example:ledger/book@1.0.0`, lives in a WIT package shared by whoever keeps a book and whoever
consults one; the `book` Orb is one keeper of it, written in Rust with `#[orb]`, and the host
installs a native keeper of the same interface next to it. The two are interchangeable: the
installer decides which Orb keeps a book, the contract stays the WIT package.

| File                       | Role                                                                                            |
| -------------------------- | ----------------------------------------------------------------------------------------------- |
| `wit/ledger.wit`           | The shared contract: the `book` interface with its record and three functions                   |
| `book/src/lib.rs`          | The compiled keeper: `#[service("example:ledger/book@1.0.0")]`, one `pub fn` per WIT function   |
| `book/contract/keeper.wit` | The contract `cargo tembra build` wrote: the shared interface exported, the kernel's state      |
| `host/src/main.rs`         | The wiring: installs both keepers and consults them the same way; defines the native keeper too |
| `run.sh`                   | Build the compiled keeper, show its contract, run the host                                      |

## Implementing instead of deriving

```rust
#[tembra::orb(package = "example:keeper@1.0.0", wit = "../wit")]
mod keeper {
    /// Implements `book` of the shared package: every function of the interface.
    #[service("example:ledger/book@1.0.0")]
    pub struct Keeper;

    impl Keeper {
        pub fn add(entry: book::Entry) -> u32 { /* ... */ }
        pub fn total() -> i64 { /* ... */ }
        pub fn entries() -> Vec<book::Entry> { /* ... */ }
    }
}
```

A plain `#[service]` derives a new interface from the `impl` block, with an identity in the Orb's
own package. `#[service("ns:name/interface@version")]` names an interface that exists already, in
the directory `wit = "../wit"` points to: the macro exports that interface in the world instead of
deriving one, re-exports its generated module under the interface's name, so the author writes
`book::Entry`, and generates the bridge from the WIT functions to the `pub fn` of the same names.
The `impl` must provide every function of the interface, with its parameter and result types; a
missing or extra `pub fn` is a compile error, and a wrong type fails to compile at the bridge.

The derived contract therefore has no interface of its own, only the world:

```wit
package example:keeper@1.0.0;

world keeper {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export example:ledger/book@1.0.0;
}
```

followed by the shared package, nested, as `cargo tembra build` writes every consumed package. The
entries are a `#[state]` of the keeper's objects, so a keeper remembers across reloads; a state is
not part of the contract unless a method of a derived service mentions it, so `Entries` is an
ordinary struct of the module, with `serde` derives the macro adds.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-shared-interface/book/Cargo.toml
```

```text
exports, the services of book:
  book::example:ledger/book@1.0.0  methods: add, total, entries
kernel-managed state: the kernel keeps the state of this Orb's service objects (tembra:guest/state@1.0.0)
imports: none; the installer grants nothing
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-shared-interface/host/Cargo.toml -- examples/rust-orb-shared-interface/book/artifacts/book.orb.wasm
```

```text
book      add(rent, -900) -> 1
book      add(salary, 2400) -> 2
book      total() -> 1500
book      entries() -> [{"amount":-900,"label":"rent"},{"amount":2400,"label":"salary"}]
notebook  add(rent, -900) -> 1
notebook  add(salary, 2400) -> 2
notebook  total() -> 1500
notebook  entries() -> [{"amount":-900,"label":"rent"},{"amount":2400,"label":"salary"}]
```

The host consults `book::example:ledger/book@1.0.0` and `notebook::example:ledger/book@1.0.0`: the
same interface, two Orbs. The native keeper is declared with the `tembra` facade and the signature
the shared package gives, so the kernel checks both implementations against one contract, and a
consumer importing `example:ledger/book@1.0.0` can be bound to either with `Install::requires`.
`run.sh` runs the three steps in order.

## Limits

An implemented interface is fixed by its WIT: the `impl` adds nothing to it, and the macro reads
the package at compile time, so the directory must hold the package the identity names.
