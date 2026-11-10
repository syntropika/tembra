# Rust Orb declaring a hook

This example runs two compiled Orbs, both written in Rust with `#[orb]` and built with
`cargo tembra`. `gate` declares a waterfall hook, `guard`, and its service `admissions` runs the
hook for each value it is asked to admit. `blocker` contributes to that hook and blocks negative
values; it has no service of its own. The host is wiring only: it installs both and drives three
admissions through the gate.

| File                           | Role                                                                                             |
| ------------------------------ | ------------------------------------------------------------------------------------------------ |
| `wit/admission.wit`            | The hook contract: the `guard` interface with its `apply` callback, shared by both Orbs          |
| `gate/src/lib.rs`              | The provider: `#[hook(..., composition = waterfall)]` on a unit struct, `Guard::run` in a method |
| `gate/contract/gate.wit`       | The contract `cargo tembra build` wrote: the hook imported and exported, the service             |
| `blocker/src/lib.rs`           | The contributor: `#[hook(..., priority = -7)]` on a `pub fn`, and nothing else                   |
| `blocker/contract/blocker.wit` | The contract of a contributor without services: the hook imported and exported                   |
| `host/src/main.rs`             | The wiring: installs the two artifacts and calls `admit` three times; it defines no Orb          |
| `run.sh`                       | Build both Orbs, show their contracts, run the host                                              |

## Declaring the hook

```rust
/// The hook this Orb declares: a waterfall, each contributor may change the admission or
/// block it, in plan order. Its callback is the `guard` interface of the shared WIT package.
#[hook("example:admission/guard@1.0.0", composition = waterfall)]
pub struct Guard;

impl Admissions {
    /// Run the guard hook over a fresh admission and report what the contributors decided.
    pub fn admit(value: i32) -> Decision {
        let decided = Guard::run(guard::Admission { value, blocked: false });
        Decision { value: decided.value, blocked: decided.blocked }
    }
}
```

A hook is a contract shared by its provider and its contributors, so it lives in a WIT package,
`wit/admission.wit`, that both crates name with `wit = "../wit"`. The provider marks a unit struct
with the hook's identity and its composition; the macro gives the struct `Guard::run(input)`, which
hands the input to the kernel. The kernel calls the contributions in plan order and returns the
composed value: for a waterfall, the admission as the last contributor left it. The composition is
checked against the callback's shape when the crate compiles: `apply` returns its input type, so it
fits a waterfall.

In the contract, the hook interface is both imported and exported, exactly as in a contributor's
world:

```wit
world gate {
  import example:admission/guard@1.0.0;
  export example:admission/guard@1.0.0;
  export admissions;
}
```

The export is what the portable hook contract requires of a provider, and the macro answers it with
a callback that traps: contributions answer the hook, the provider never does. The import is the
typed entry to the plan; `Guard::run` calls it, and the executor turns the call into the kernel's
`hook.run`, the same request a TypeScript provider makes. What tells the two roles apart is the
`tembra:hooks` section the macro writes into the component, `declares` for the gate and
`contributes` for the blocker; `cargo tembra build` reads it and packages the role in the artifact,
and the kernel binds the gate's import to the plan instead of asking the installer for a provider.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-hook-declaration/gate/Cargo.toml
cargo tembra build --manifest-path examples/rust-orb-hook-declaration/blocker/Cargo.toml
```

The gate's summary names the hook it declares; the blocker's names its contribution and no service:

```text
exports, the services of gate:
  gate::example:gate/admissions@1.0.0  methods: admit
hooks declared, run by the kernel's plan of contributions:
  example:admission/guard@1.0.0  waterfall
other imports: none; the installer grants nothing

exports, the services of blocker:
hook contributions, bound to the provider with Install::requires:
  example:admission/guard@1.0.0  priority -7
other imports: none; the installer grants nothing
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-hook-declaration/host/Cargo.toml -- examples/rust-orb-hook-declaration/gate/artifacts/gate.orb.wasm examples/rust-orb-hook-declaration/blocker/artifacts/blocker.orb.wasm
```

```text
admit(5) -> {"blocked":false,"value":5}
admit(-3) -> {"blocked":true,"value":-3}
admit(8) -> {"blocked":false,"value":8}
```

The host loads the gate with `Install::default()`: declaring a hook requires and grants nothing.
It loads the blocker with `Install::default().requires("example:admission/guard@1.0.0", "gate", 1)`,
which binds the contributor's imported hook interface to the `gate` Orb. Each `admit` runs the
plan; the blocker sees every admission and blocks the negative one. `run.sh` runs the steps in order.

## Limits

The composition is one of `series`, `waterfall` and `bail`, as for TypeScript providers; custom
composition and observation are not exposed to compiled Orbs. `Guard::run` runs inside a method
call, because the kernel runs a plan under an admitted invocation. Service signatures use the
module's own types: `admit` returns the module's `Decision`, converted from the hook's `Admission`,
because the macro does not yet place a consumed package's types in a service interface.
