# Rust Orb with kernel-managed state

This example writes a compiled Orb whose state belongs to the kernel, not to the component. The
`counter` Orb has one service, `counter`, and one state, `Count`. Its methods read and update the
state through the kernel; the host observes the state through a reference, with no method call,
and reloads the Orb with a fresh component instance while the count stays.

| File                         | Role                                                                                    |
| ---------------------------- | --------------------------------------------------------------------------------------- |
| `counter/src/lib.rs`         | The Orb: `#[state]` on a struct, `Count::read()` and `Count::update(..)` in the methods |
| `counter/contract/tally.wit` | The contract `cargo tembra build` wrote: the kernel's state interfaces in the world     |
| `host/src/main.rs`           | Application code: subscribes to the state, calls `add`, reloads, calls again            |
| `run.sh`                     | Build, show the contract, run the host                                                  |

## The state

```rust
/// The state of one counter object: its total and the number of additions made.
#[state(version = 1)]
pub struct Count {
    pub total: i32,
    pub steps: u32,
}
```

`#[state]` marks the struct as the state of the objects of a service: the module's only one here,
otherwise `#[state(service = Counter)]`. The kernel keeps exactly one state per object, so a service
has one state struct; `version` is what the kernel compares when the Orb is reloaded. The macro
generates `Count::read()` and `Count::update(|count| ...)`, and the initial value is
`Count::default()`, implemented field by field. The world gains two lines:

```wit
world tally {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export counter;
}
```

`tembra:guest` is the kernel's own package, bundled with `tembra-contracts` like WASI. When the
kernel creates a `counter` object it calls the exported `state-init.declare` with the service's
interface name, gets the initial JSON and the version back, and declares the state for that object;
`read` and `update` then reach the state of the object running the call. The executor serves the
import itself: the installer requires and grants nothing.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-state/counter/Cargo.toml
```

The summary names the state next to the service:

```text
exports, the services of counter:
  counter::example:counter/counter@1.0.0  methods: add, read
kernel-managed state: the kernel keeps the state of this Orb's service objects (tembra:guest/state@1.0.0)
imports: none; the installer grants nothing
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-state/host/Cargo.toml -- examples/rust-orb-state/counter/artifacts/counter.orb.wasm
```

```text
observed state      {"steps":0,"total":0}
observed state      {"steps":1,"total":5}
add(5)           -> {"steps":1,"total":5}
observed state      {"steps":2,"total":3}
add(-2)          -> {"steps":2,"total":3}
reloaded            component instances: 1
read()           -> {"steps":2,"total":3}
observed state      {"steps":3,"total":4}
add(1)           -> {"steps":3,"total":4}
```

The `observed state` lines come from `reference.state::<Value>()?.subscribe(..)` in the host: the
first with the initial value, then one per update the kernel adopts, before the method returns. The
reload installs a second load of the same artifact as a new revision: the old component instance
is retired (`instances: 1` once the handover settles), the object and its state remain, and `read`
returns the count the previous instance left. A count kept in a `static` would have restarted from
zero. `run.sh` runs the three steps in order.

## Limits

A state belongs to one service object and is reached only by that service's methods: a call runs
on one object, and the kernel refuses an update from any other context. The Orb's root is not an
object, so a hook contribution has no kernel state; a root resource is what it can use, see
[the hooks example](../rust-orb-hooks/README.md#why-the-counts-are-a-resource-not-a-state). The
kernel stores the state as JSON written by `serde`, with the Rust field names.
