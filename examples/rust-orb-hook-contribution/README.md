# Rust Orb contributing to a hook

This example runs two Orbs. A native Orb, `gate`, declares a waterfall hook and keeps a ledger of
the admissions it decided. A compiled Orb, `filter`, written in Rust with `#[orb]` and built with
`cargo tembra`, contributes to that hook, blocks negative values and counts what it saw in a
resource of its root that the kernel opens and closes. The host is wiring only: it installs both,
drives three admissions, reloads the contributor and drives one more.

| File                         | Role                                                                                             |
| ---------------------------- | ------------------------------------------------------------------------------------------------ |
| `wit/admission.wit`          | The hook contract: the `guard` interface with its `apply` callback, exported by `provider`       |
| `gate/src/lib.rs`            | The native provider, a crate of its own: the `guard` hook with the WIT signature and a ledger    |
| `filter/src/lib.rs`          | The compiled contributor: `#[hook]` on a `pub fn`, counts in a `#[resource]`, a `report` service |
| `filter/contract/filter.wit` | The contract `cargo tembra build` wrote: the hook imported and exported, the kernel's resources  |
| `host/src/main.rs`           | The wiring: installs the two Orbs, drives admissions, reloads the contributor; it defines no Orb |
| `run.sh`                     | Build, show the contract, run the host                                                           |

## Why the provider is native here

A compiled Orb can declare a hook too; [rust-orb-hook-declaration](../rust-orb-hook-declaration/README.md)
shows it. This example keeps the provider native on purpose: a sandboxed contributor plugging into
a hook of trusted application code is the case of an application extended by plugins. The provider
is declared with the `tembra` facade, as in [examples/rust/hooks.rs](../rust/hooks.rs), and attaches
the WIT callback signature to its hook with `Hook::signature`, so the compiled contributor is
checked against the same contract at installation.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-hook-contribution/filter/Cargo.toml
```

The summary lists the contribution and the resources next to the service:

```text
exports, the services of filter:
  filter::example:filter/report@1.0.0  methods: read
hook contributions, bound to the provider with Install::requires:
  example:admission/guard@1.0.0  priority -7
kernel-managed resources: the kernel opens and closes the resources of this Orb's root (tembra:guest/resources@1.0.0)
```

The priority comes from the attribute, `#[hook("example:admission/guard@1.0.0", priority = -7)]`,
and travels in the component like the contract. The contract in `filter/contract/filter.wit`
imports and exports `example:admission/guard@1.0.0`, as the portable hook contract requires,
although the component never calls the import: the macro wrote that contract into the component,
and the linker is free to drop the unused import.

## Run

```sh
cargo run --manifest-path examples/rust-orb-hook-contribution/host/Cargo.toml -- examples/rust-orb-hook-contribution/filter/artifacts/filter.orb.wasm
```

```text
admit(5) -> {"blocked":false,"value":5}
admit(-3) -> {"blocked":true,"value":-3}
admit(8) -> {"blocked":false,"value":8}
gate ledger -> {"admitted":[5,8],"blocked":1}
filter report -> {"blocked":1,"seen":3}
filter reloaded
admit(-1) -> {"blocked":true,"value":-1}
gate ledger -> {"admitted":[5,8],"blocked":2}
filter report -> {"blocked":1,"seen":1}
```

The host loads the artifact with `Install::default().requires("example:admission/guard@1.0.0", "gate", 1)`:
the imported hook interface is bound to the `gate` Orb like any other required import. Each
`admit` runs the hook; the contributor transforms the admission and the provider records the
decision. Then the host reloads `filter` with a second load of the same artifact: a new component
instance. The ledger of the native `gate` continues, the counts of `filter` start again, because
its resource was reopened in the new heap. `run.sh` runs the three steps in order.

## Why the counts are a resource, not a state

The kernel keeps state for service objects, and a contribution runs in its Orb's root, which is not
an object: there is no kernel state a contribution could update, in a compiled Orb as in a
TypeScript one. What the root can own is a **resource**: a value the kernel opens when the Orb is
activated and closes when it is torn down, in dependency order. `filter` declares one:

```rust
#[resource]
pub struct Counts { pub seen: u32, pub blocked: u32 }

impl Counts {
    #[open]
    pub fn open() -> Counts { Counts { seen: 0, blocked: 0 } }
}
```

The world gains `import tembra:guest/resources` and `export tembra:guest/resources-init`. When the
kernel creates the root it asks the component for its declarations, then runs the function marked
`#[open]` during activation and the one marked `#[close]`, if any, at teardown;
`Counts::with(|counts| ...)` asks the kernel for the handle of
the open resource and uses the value, which lives in the component. That is the same model as a
TypeScript root resource: the kernel manages the lifecycle, the guest holds the value, and the
value does not cross a reload. A kernel-managed state would survive the reload, but only a service's
own methods can update one; see the [state example](../rust-orb-state/README.md).
