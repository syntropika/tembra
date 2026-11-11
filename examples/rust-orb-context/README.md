# Rust Orb reading its call's context

This example writes a compiled Orb that asks the kernel about the call it is serving: who is
calling, which object answers, whether the call was cancelled, and it writes to the host's log.
The `audit` Orb has one service, `auditor`; `context` on its `#[orb]` attribute gives the module a
`context` module of functions the kernel answers through its private `tembra:orb/host` interface,
which the host links itself. The host is wiring only: it installs the Orb, observes its log and
calls it through two references acquired by different principals with different keys.

| File                       | Role                                                                                                 |
| -------------------------- | ---------------------------------------------------------------------------------------------------- |
| `audit/src/lib.rs`         | The Orb: `context` on the attribute, `context::principal()`, `context::object()`, `context::log(..)` |
| `audit/contract/audit.wit` | The contract `cargo tembra build` wrote: the `auditor` interface; the context is not part of it      |
| `host/src/main.rs`         | The wiring: observes the log, acquires two references, calls the methods; it defines no Orb          |
| `run.sh`                   | Build, show the contract, run the host                                                               |

## The context

```rust
#[tembra_orb_macro::orb(package = "example:audit@1.0.0", context)]
mod audit {
    impl Auditor {
        /// The call's principal and the address of the object running it; a log record too.
        pub fn whoami() -> Caller {
            let object = context::object().expect("the kernel knows the object it runs");
            context::log(context::Level::Info, "whoami was asked");
            Caller {
                principal: context::principal().expect("a running call has a principal"),
                key: object.key,
                owner: object.principal,
            }
        }
    }
}
```

A method runs inside a call the kernel admitted, on an object the kernel created for a key and a
principal. `context::principal()` is the principal of the call, the one the host's root was opened
with. `context::object()` is the address of the object the call runs on: the key its reference was
acquired with and the principal that acquired it, as the kernel gave them when it created the
object and the generated `lifecycle.create` kept them; in a hook contribution, which runs in the
Orb's root, the key is empty and the principal is `host`. `context::cancelled()` says whether the
call was cancelled and `context::cancel()` cancels it. `context::log(level, message)` hands a
record to the host's log observers, attributed to the Orb by the host, never by the guest.

Every function but `log` returns a `Result`: after a cancellation the kernel refuses `principal`
and `cancel`, and the method decides what a refusal means. The methods here expect the answers a
live call always gets, and `give_up`, which cancels its own call first, counts a refused
`cancelled()` as cancelled.

The contract does not change: the functions reach the kernel through `tembra:orb/host`, which the
host links itself, so the world exports the service alone and the installer requires and grants
nothing for the context.

```wit
world audit {
  export auditor;
}
```

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-context/audit/Cargo.toml
```

The summary names the kernel's private package, which answers the context, next to the service:

```text
exports, the services of audit:
  audit::example:audit/auditor@1.0.0  methods: whoami, warn, give-up
kernel lifecycle: the host describes, creates and releases this Orb through tembra:orb/lifecycle@0.1.0 and answers its state, resources, hooks and context through tembra:orb/host@0.1.0
imports: none; the installer grants nothing
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-context/host/Cargo.toml -- examples/rust-orb-context/audit/artifacts/audit.orb.wasm
```

```text
log            [info] audit: whoami was asked
alice, room 7  whoami() -> {"key":["room","7"],"owner":"alice","principal":"alice"}
log            [info] audit: whoami was asked
bob, desk      whoami() -> {"key":["desk"],"owner":"bob","principal":"bob"}
log            [warn] audit: the desk is full
bob, desk      give-up() -> true
```

The host installs the engine with `Engine::new(..).on_log(..)`, which receives every record a
compiled Orb writes; the `log` lines are printed by that observer, before the method returns. Then
it acquires two objects of the same service, `["room", "7"]` for `alice` and `["desk"]` for `bob`,
and each `whoami` reports the principal of its root and the address of its object. `warn` writes at
level warn. `give-up` cancels its own call from inside and reports what the kernel then says.
Methods are called by their WIT names: `give_up` in Rust is `give-up` in the contract. `run.sh`
runs the three steps in order.

## Limits

The principal and the cancellation belong to a call: outside one, in a resource's `#[open]`
function for instance, the kernel refuses them and the functions return its refusal. The context
tells an Orb about the object running the call and about the caller's principal; selecting another
object by key, as a TypeScript Orb does with `context.get`, is not expressed by the macro yet.
