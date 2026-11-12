# Rust Orb with kernel-managed state

This example writes a compiled Orb whose state belongs to the kernel, not to the component. The
`counter` Orb has one service, `counter`, and one state, `Count`, with a rule of its own. Its
methods read and update the state through the kernel, which stores only what the rule accepts;
the host observes the state through a reference, with no method call, sees an update refused, and
reloads the Orb with a fresh component instance while the count stays.

| File                         | Role                                                                                                   |
| ---------------------------- | ------------------------------------------------------------------------------------------------------ |
| `counter/src/lib.rs`         | The Orb: `#[state]` on a struct with a `#[validate]` function, `Count::read()` and `Count::update(..)` |
| `counter/contract/tally.wit` | The contract `cargo tembra build` wrote: the kernel's state interfaces in the world                    |
| `host/src/main.rs`           | The wiring: subscribes to the state, calls `add`, reloads, calls again; it defines no Orb              |
| `run.sh`                     | Build, show the contract, run the host                                                                 |

## The state

```rust
/// The state of one counter object: its total and the number of additions made.
#[state(version = 1)]
pub struct Count {
    pub total: i64,
    pub steps: u32,
}
```

`#[state]` marks the struct as the state of the objects of a service: the module's only one here,
otherwise `#[state(service = Counter)]`. The kernel keeps exactly one state per object, so a service
has one state struct; `version` is what the kernel compares when the Orb is reloaded. The macro
generates `Count::read()` and `Count::update(|count| ...)`, and the initial value is
`Count::default()`, implemented field by field. `total` is an `i64`: the WIT says `s64`, and the
executor carries 64-bit integers exactly across the boundary, so the host can add five billion.

```rust
impl Count {
    /// The rule of this state: a total never goes below zero.
    #[validate]
    pub fn validate(&self) -> Result<(), String> {
        if self.total < 0 {
            return Err(format!("a total of {} is below zero", self.total));
        }
        Ok(())
    }
}
```

A state may carry its rule: a function of its `impl` block marked `#[validate]`, of the shape
`fn(&self) -> Result<(), String>`, under any name. Then `Count::update` returns
`Result<Count, String>`: the closure's result is validated before the kernel stores it, and a
refusal leaves the previous value in place, so `add` returns a `result<count, string>` in the WIT.
The same function answers the kernel on a reload, when the new instance is asked whether it
accepts the value it inherits; a refusal fails the reload and the running revision keeps serving.
Without the marker, a function named `validate` is ordinary Rust, and a reload still checks that
the inherited value reads as a `Count`. The world gains two lines:

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
`read` and `update` then reach the state of the object running the call, and `state-init.validate`
is what a reload calls: the new instance reads the value it inherits as a `Count` and runs the rule
on it. The executor serves the import itself: the installer requires and grants nothing.

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
observed state      {"steps":1,"total":5000000000}
add(5000000000)  -> ok {"steps":1,"total":5000000000}
observed state      {"steps":2,"total":4999999998}
add(-2)          -> ok {"steps":2,"total":4999999998}
add(-6000000000) -> refused: a total of -1000000002 is below zero
reloaded            component instances: 1
read()           -> {"steps":2,"total":4999999998}
observed state      {"steps":3,"total":4999999999}
add(1)           -> ok {"steps":3,"total":4999999999}
```

The `observed state` lines come from `reference.state::<Value>()?.subscribe(..)` in the host: the
first with the initial value, then one per update the kernel adopts, before the method returns. The
refused addition reaches no observer: the validator stopped it in the component, before the kernel
saw a new value, and `add` reports it as the `err` case of its WIT `result`. The reload installs a
second load of the same artifact as a new revision: the new instance validates the count it
inherits, the old component instance is retired (`instances: 1` once the handover settles), the
object and its state remain, and `read` returns the count the previous instance left. A count kept
in a `static` would have restarted from zero. `run.sh` runs the three steps in order. The
executor's `compiled_state` test shows the other outcome of a reload, a value the new instance
refuses, with the running revision kept.

## Limits

A state belongs to one service object and is reached only by that service's methods: a call runs
on one object, and the kernel refuses an update from any other context. The Orb's root is not an
object, so a hook contribution has no kernel state; a root resource is what it can use, see
[the hook contribution example](../rust-orb-hook-contribution/README.md#why-the-counts-are-a-resource-not-a-state). The
kernel stores the state as JSON written by `serde`, with the Rust field names.
