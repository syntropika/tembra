# Rust Orb contributing to a hook

This example runs two Orbs with state. A native Orb, `gate`, declares a waterfall hook and keeps a
ledger of the admissions it decided. A compiled Orb, `filter`, written in Rust with `#[orb]` and
built with `cargo tembra`, contributes to that hook, blocks negative values and counts what it saw.
The host installs both and drives three admissions.

| File                         | Role                                                                                                |
| ---------------------------- | --------------------------------------------------------------------------------------------------- |
| `wit/admission.wit`          | The hook contract: the `guard` interface with its `apply` callback, exported by `provider`          |
| `filter/src/lib.rs`          | The compiled contributor: `#[contributes]` on a `pub fn`, state in statics, a `report` service      |
| `filter/contract/filter.wit` | The contract `cargo tembra build` wrote: the hook imported and exported, `example:admission` nested |
| `host/src/main.rs`           | The native `gate` Orb with its hook and ledger, and the application code installing both            |
| `run.sh`                     | Build, show the contract, run the host                                                              |

## Why the provider is native

Compiled Orbs can contribute to hooks but cannot declare them yet; see
[portable hook contracts](../../docs/extensions.md#portable-hook-contracts). The provider is
therefore trusted application code declared with the `tembra` facade, as in
[examples/rust/hooks.rs](../rust/hooks.rs). It attaches the WIT callback signature to its hook with
`Hook::signature`, so the compiled contributor is checked against the same contract at installation.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-hooks/filter/Cargo.toml
```

The summary lists the contribution next to the service:

```text
exports, the services of filter:
  filter::example:filter/report@1.0.0  methods: read
hook contributions, bound to the provider with Install::requires:
  example:admission/guard@1.0.0  priority -7
```

The priority comes from `[package.metadata.tembra] contributes` in `filter/Cargo.toml`. The
contract in `filter/contract/filter.wit` imports and exports `example:admission/guard@1.0.0`, as the
portable hook contract requires, although the component never calls the import: the macro wrote
that contract into the component, and the linker is free to drop the unused import.

## Run

```sh
cargo run --manifest-path examples/rust-orb-hooks/host/Cargo.toml -- examples/rust-orb-hooks/filter/artifacts/filter.orb.wasm
```

```text
admit(5) -> {"blocked":false,"value":5}
admit(-3) -> {"blocked":true,"value":-3}
admit(8) -> {"blocked":false,"value":8}
gate ledger -> {"admitted":[5,8],"blocked":1}
filter report -> {"blocked":1,"seen":3}
```

The host loads the artifact with `Install::default().requires("example:admission/guard@1.0.0", "gate", 1)`:
the imported hook interface is bound to the `gate` Orb like any other required import. Each
`admit` runs the hook; the contributor transforms the admission and the provider records the
decision. The two states diverge as expected: the ledger belongs to `gate`, the counts to `filter`,
which keeps them in statics for as long as it stays installed. `run.sh` runs the three steps in
order.
