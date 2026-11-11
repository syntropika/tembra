# Rust Orb choosing its services' lifetime

This example writes a compiled Orb with three services of one shape and three lifetimes. Each
`add` method changes a balance the kernel keeps as the object's state, so what the balance shows
after every reference was released tells what the lifetime means. The host is wiring only: for
each service it adds, releases, acquires again and adds again.

| File                         | Role                                                                                                         |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------ |
| `ledger/src/lib.rs`          | The Orb: three sub-modules, each a service with its `#[state]`; `lifetime = host`, `linger_ms = 2500`, plain |
| `ledger/contract/ledger.wit` | The contract `cargo tembra build` wrote: three interfaces of one shape; no lifetime, no state                |
| `host/src/main.rs`           | The wiring: acquire, add, release, acquire again, add again, per service; it defines no Orb                  |
| `run.sh`                     | Build, show the contract, run the host                                                                       |

## The three lifetimes

```rust
/// A ledger whose `main` object the host creates when it installs the Orb and keeps until it
/// closes; it is acquired only as `main` by the principal `host`.
#[service(lifetime = host)]
pub struct Pinned;

/// A ledger whose objects stay two and a half seconds after their last reference is released.
#[service(linger_ms = 2500)]
pub struct Lingering;

/// A ledger whose objects live exactly while something references them: the default.
#[service]
pub struct Passing;
```

The kernel creates one object per key and principal a reference names, and by default keeps it
while something references it: when the last reference is released the object goes, and its state
with it. `linger_ms = n` keeps an unreferenced object for `n` milliseconds, so a reference taken
again soon finds the same object and the same balance. `lifetime = host` pins the object: the host
creates the `main` object under its own principal when it installs the Orb and nothing releases it
before the host closes; such an object is acquired only as `main` by the principal `host`, and a
pinned object cannot linger.

The lifetime is not WIT: the derived contract shows three ordinary interfaces. It travels in the
component's `lifecycle.describe`, which reports each service with its lifetime and its linger, as
the description of a TypeScript Orb does, and the kernel applies it.

## One sub-module per service

```rust
mod ledger {
    pub mod pinned {
        /// The balance of one pinned ledger object, kept by the kernel.
        #[state]
        pub struct Balance { pub total: i64 }

        #[service(lifetime = host)]
        pub struct Pinned;

        impl Pinned {
            pub fn add(amount: i64) -> i64 {
                Balance::update(|balance| balance.total += amount)
                    .expect("the kernel keeps the balance")
                    .total
            }
        }
    }

    pub mod lingering { /* its own Balance, Lingering */ }
    pub mod passing { /* its own Balance, Passing */ }
}
```

The three services are alike, so each lives in a sub-module with its state. A state that names no
service belongs to the one service of its module, so none of the three writes
`#[state(service = ..)]`; and since a balance stays out of the WIT, `add` returns the total, the
three states share the name `Balance`, each in its module. `update` returns the stored value or the
kernel's refusal; a balance has no rule of its own, so `add` expects the value. The sub-modules are
read as part of the Orb and left in place: the derived contract is the same as with everything at
one level. A state of a sub-module is `pub` with `pub` fields, because the Orb module builds and
reads it.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-service-lifetime/ledger/Cargo.toml
```

The summary lists the three services alike: their lifetimes travel in `lifecycle.describe`, which
the summary does not print.

```text
exports, the services of ledger:
  ledger::example:ledger/pinned@1.0.0  methods: add
  ledger::example:ledger/lingering@1.0.0  methods: add
  ledger::example:ledger/passing@1.0.0  methods: add
kernel lifecycle: the host describes, creates and releases this Orb through tembra:orb/lifecycle@0.1.0 and answers its state, resources, hooks and context through tembra:orb/host@0.1.0
imports: none; the installer grants nothing
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-service-lifetime/host/Cargo.toml -- examples/rust-orb-service-lifetime/ledger/artifacts/ledger.orb.wasm
```

```text
pinned     add(5) -> 5   released, acquired again   add(1) -> 6
lingering  add(5) -> 5   released, acquired again   add(1) -> 6
passing    add(5) -> 5   released, acquired again   add(1) -> 1
```

The pinned ledger kept its balance: the host's own reference, taken at installation, kept the
object. The lingering one kept it too, because the second reference came within its linger. The
passing one lost it: its object went with the released reference, and the second reference created
a new object with a fresh balance. `run.sh` runs the three steps in order.

## Limits

A host-lifetime object is acquired as `main` by the principal `host` and by nobody else: the first
`host.get` of the example says so. The linger is driven by the host's clock; this host does not
wait it out, so the expiry of the lingering object is not shown.
