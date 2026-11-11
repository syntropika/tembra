# Rust Orb choosing its services' lifetime

This example writes a compiled Orb with three services of one shape and three lifetimes. Each
`add` method changes a balance the kernel keeps as the object's state, so what the balance shows
after every reference was released tells what the lifetime means. The host is wiring only: for
each service it adds, releases, acquires again and adds again.

| File                         | Role                                                                                            |
| ---------------------------- | ----------------------------------------------------------------------------------------------- |
| `ledger/src/lib.rs`          | The Orb: `#[service(lifetime = host)]`, `#[service(linger_ms = 2500)]` and a plain `#[service]` |
| `ledger/contract/ledger.wit` | The contract `cargo tembra build` wrote: three interfaces, the kernel's state interfaces        |
| `host/src/main.rs`           | The wiring: acquire, add, release, acquire again, add again, per service; it defines no Orb     |
| `run.sh`                     | Build, show the contract, run the host                                                          |

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
component's `tembra:manifest` section with the hook roles, keyed by the interface identity, and
`cargo tembra build` packages it in the artifact, where the executor reads it as a TypeScript
service's own `lifetime` and `linger` declaration.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-service-lifetime/ledger/Cargo.toml
```

The summary names the lifetime next to each service that chose one:

```text
exports, the services of ledger:
  ledger::example:ledger/pinned@1.0.0  methods: add  lifetime: host, pinned until the host closes
  ledger::example:ledger/lingering@1.0.0  methods: add  linger: 2500 ms after the last reference is released
  ledger::example:ledger/passing@1.0.0  methods: add
kernel-managed state: the kernel keeps the state of this Orb's service objects (tembra:guest/state@1.0.0)
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
