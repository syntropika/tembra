# Rust Orb declaring a hook

This example runs two compiled Orbs, both written in Rust with `#[orb]` and built with
`cargo tembra`. `gate` declares a waterfall hook, `guard`, from its own code, and its service
`admissions` runs the hook for each value it is asked to admit. `blocker` reads the gate's contract,
contributes to that hook and blocks negative values; it has no service of its own. The host is
wiring only: it installs both and drives three admissions through the gate.

| File                           | Role                                                                                                            |
| ------------------------------ | --------------------------------------------------------------------------------------------------------------- |
| `gate/src/lib.rs`              | The provider: `#[hook(composition = waterfall, input = Admission)]` on a unit struct, `Guard::run` in a method  |
| `gate/contract/gate.wit`       | The contract `cargo tembra build` wrote: the hook interface derived from the struct, the service                |
| `blocker/src/lib.rs`           | The contributor: `wit = "../gate/contract"`, `#[hook("example:gate/guard@1.0.0", priority = -7)]` on a `pub fn` |
| `blocker/contract/blocker.wit` | The contract of a contributor without services: the hook imported and exported, the gate's package nested       |
| `host/src/main.rs`             | The wiring: installs the two artifacts and calls `admit` three times; it defines no Orb                         |
| `run.sh`                       | Build the gate, then the blocker against its contract, show both contracts, run the host                        |

## Declaring the hook

```rust
/// The hook this Orb declares: a waterfall, each contributor may change the admission or
/// block it, in plan order. Its interface is derived from this struct and from `Admission`.
#[hook(composition = waterfall, input = Admission)]
pub struct Guard;

/// One admission: the value, and whether a contributor blocked it.
pub struct Admission { pub value: i32, pub blocked: bool }

impl Admissions {
    /// Run the guard hook over a fresh admission and report what the contributors decided.
    pub fn admit(value: i32) -> Admission {
        Guard::run(Admission { value, blocked: false })
    }
}
```

A hook declared this way is derived from the module like a service: the struct names the hook,
`guard`, the composition says how the kernel combines the contributions, and `input` names the type
the contributors receive and, for a waterfall, return. The macro writes the hook's interface into
the gate's own package, `example:gate/guard@1.0.0`, with the record and one callback, `apply`,
whose result follows the composition: nothing for a series hook, the input type for a waterfall, an
option for a bail. No WIT is written by hand, and `Admission` is one type: `admit` returns the
value the hook composed.

```wit
interface guard {
  record admission {
    value: s32,
    blocked: bool,
  }
  apply: func(input: admission) -> admission;
}

interface admissions {
  use guard.{admission};
  admit: func(value: s32) -> admission;
}

world gate {
  import guard;
  export guard;
  export admissions;
}
```

The macro gives the struct `Guard::run(input)`, which hands the input to the kernel; the kernel
calls the contributions in plan order and returns the composed value, for a waterfall the admission
as the last contributor left it. In the contract the hook interface is both imported and exported.
The export is what the portable hook contract requires of a provider, and the macro answers it with
a callback that traps: contributions answer the hook, the provider never does. The import is the
typed entry to the plan; `Guard::run` calls it, and the executor turns the call into the kernel's
`hook.run`, the same request a TypeScript provider makes. The bindings spell the record once per
side, as two Rust types; the macro generates the conversions between them, so the author sees one.
Which role an interface plays travels in the `tembra:manifest` section the macro writes into the
component, `declares` for the gate and `contributes` for the blocker; `cargo tembra build` reads it
and packages the role in the artifact, and the kernel binds the gate's import to the plan instead of
asking the installer for a provider.

## Contributing from the gate's contract

```rust
#[tembra::orb(package = "example:blocker@1.0.0", wit = "../gate/contract")]
mod blocker {
    /// Block negative values; let the others continue down the hook unchanged.
    #[hook("example:gate/guard@1.0.0", priority = -7)]
    pub fn block_negative(input: guard::Admission) -> guard::Admission { /* ... */ }
}
```

The hook's contract is born in the gate's build, so the blocker reads it from there, as a consumer
reads any interface of another Orb; see [rust-orb-import](../rust-orb-import/README.md). The
contributor's own contract nests the gate's package whole. When a hook is a contract that no single
Orb owns, or when its provider is native, its WIT is written by hand and the provider names it,
`#[hook("ns:name/hook@version", composition = waterfall)]` under `wit = "../wit"`, as
[rust-orb-hook-contribution](../rust-orb-hook-contribution/README.md) shows with a native gate.

## Build

Run from the repository root; the blocker is built against the gate's contract, so the gate comes
first.

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
  example:gate/guard@1.0.0  waterfall
other imports: none; the installer grants nothing

exports, the services of blocker:
hook contributions, bound to the provider with Install::requires:
  example:gate/guard@1.0.0  priority -7
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

The host loads the gate with `Install::default()`: declaring a hook requires and grants nothing. It
loads the blocker with `Install::default().requires("example:gate/guard@1.0.0", "gate", 1)`, which
binds the contributor's imported hook interface to the `gate` Orb. Each `admit` runs the plan; the
blocker sees every admission and blocks the negative one. `run.sh` runs the steps in order.

## Limits

The composition is one of `series`, `waterfall` and `bail`, as for TypeScript providers; custom
composition and observation are not exposed to compiled Orbs. `Guard::run` runs inside a method
call, because the kernel runs a plan under an admitted invocation. The input of a derived hook is a
record, an enum, a variant, a scalar, a string or a list; the types it reaches are defined in the
hook's interface, and the services borrow them with `use`.
