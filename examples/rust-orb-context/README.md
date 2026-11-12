# Rust Orb reading its call's context

This example writes a compiled Orb that asks the kernel about the call it is serving: who is
calling, which object answers, whether the call was cancelled, and it writes to the host's log.
The `audit` Orb has one service, `auditor`; `context` on its `#[orb]` attribute imports the
kernel's `tembra:guest/context` interface, which the executor serves itself. The host is wiring
only: it installs the Orb, observes its log and calls it through two references acquired by
different principals with different keys.

| File                       | Role                                                                                                 |
| -------------------------- | ---------------------------------------------------------------------------------------------------- |
| `audit/src/lib.rs`         | The Orb: `context` on the attribute, `context::principal()`, `context::object()`, `context::log(..)` |
| `audit/contract/audit.wit` | The contract `cargo tembra build` wrote: the kernel's context interface imported in the world        |
| `host/src/main.rs`         | The wiring: observes the log, acquires two references, calls the methods; it defines no Orb          |
| `run.sh`                   | Build, show the contract, run the host                                                               |

## The context

```rust
#[tembra::orb(package = "example:audit@1.0.0", context)]
mod audit {
    impl Auditor {
        /// The call's principal and the address of the object running it; a log record too.
        pub fn whoami() -> Caller {
            let object = context::object();
            context::log(context::Level::Info, "whoami was asked");
            Caller { principal: context::principal(), key: object.key, owner: object.principal }
        }
    }
}
```

A method runs inside a call the kernel admitted, on an object the kernel created for a key and a
principal. `context::principal()` is the principal of the call, the one the host's root was opened
with. `context::object()` is the address of the object the call runs on: the key its reference was
acquired with and the principal that acquired it; in a hook contribution, which runs in the Orb's
root, the key is empty and the principal is `host`. `context::cancelled()` says whether the call
was cancelled and `context::cancel()` cancels it. `context::log(level, message)` hands a record to
the host's log observers, attributed to the Orb by the host, never by the guest.

The world gains one line and no export: the executor answers the interface itself, so the
installer requires and grants nothing for it.

```wit
world audit {
  import tembra:guest/context@1.0.0;
  export auditor;
}
```

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-context/audit/Cargo.toml
```

The summary names the kernel-served interface next to the service:

```text
exports, the services of audit:
  audit::example:audit/auditor@1.0.0  methods: whoami, warn, give-up
kernel-served context: the call's principal and cancellation, the object's address and the log (tembra:guest/context@1.0.0)
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

The principal and the cancellation belong to a call: outside one, in a resource's `open` for
instance, asking for them traps the component. The context tells an Orb about the object running
the call and about the caller's principal; another Orb's object of a key, which a TypeScript Orb
selects with `context.get`, is reached by
[importing its interface by key](../../crates/orb-macro/README.md#importing-another-orbs-interface-by-key).
