# Rust Orb with resources of a service's objects

This example writes a compiled Orb whose service objects own resources. The `harbor` Orb has one
resource of its root, `Harbor`, opened once when the Orb is activated, and one service, `ship`,
each of whose objects owns a `Mooring`, opened when the kernel creates the ship and closed when
the ship goes. The mooring depends on the harbor: its `#[open]` function takes the harbor's next
berth and its `#[close]` function gives it back. The host moors two ships, sees the third refused
when every berth is taken, releases a ship and watches its mooring close, then moors a new ship
at a fresh berth. The same module serves a native host: `run.sh` drives the compiled Orb, loaded
from its artifact, and then the native Orb the module's `orb()` builds, through one host that
prints the same lines for both.

| File                       | Role                                                                                                      |
| -------------------------- | --------------------------------------------------------------------------------------------------------- |
| `harbor/src/lib.rs`        | The Orb: a `#[resource]` outside every service's module, and one inside the `ship` module                 |
| `harbor/contract/port.wit` | The contract `cargo tembra build` wrote: the kernel's resource and context interfaces in the world        |
| `host/src/main.rs`         | The wiring: creates ships, calls them, releases one; the compiled Orb or, with `--native`, the native one |
| `run.sh`                   | Build, show the contract, run the host on the compiled Orb, then on the native one                        |

## The resources

```rust
/// The harbor: a resource of the Orb's root, since it sits outside every service's module.
#[resource]
pub struct Harbor { pub berths: u32, pub moored: u32, pub issued: u32, pub log: Vec<String> }

pub mod ship {
    #[service]
    pub struct Ship;

    /// The mooring of one ship: a resource of each `Ship` object, since it sits in the
    /// service's module.
    #[resource(depends = [Harbor])]
    pub struct Mooring { pub berth: u32, pub cargo: u32 }

    impl Mooring {
        #[open]
        pub fn open() -> Result<Mooring, String> { /* take the harbor's next berth */ }

        #[close]
        pub fn close(self) { /* give it back */ }
    }
}
```

Where a `#[resource]` struct sits says whose it is: outside every service's module it is the
root's, one per Orb; in a service's module it is the service's, one per object. Both are written
the same way, with a function marked `#[open]`, `fn() -> Self` or `fn() -> Result<Self, E>`, and
optionally one marked `#[close]`, `fn(self)`. `depends = [Harbor]` names the resource that opens
before the mooring and closes after it, so both marked functions may use the harbor with
`Harbor::with(|harbor| ..)`. In a method of the service, `Mooring::with(|mooring| ..)` gives the
mooring of the object running the call: `berth` and `load` read and change the mooring of their
own ship. An `Err` from `#[open]` fails the creation of that ship alone, with the code
`unavailable`, the reason `resource` and the error's text. The world gains two lines for the
root's resources, one for the service's and, with the `context` option, the context's import:

```wit
world port {
  import tembra:guest/resources@1.0.0;
  export tembra:guest/resources-init@1.0.0;
  export tembra:guest/service-resources-init@1.0.0;
  import tembra:guest/context@1.0.0;
  export ship;
}
```

`tembra:guest` is the kernel's own package, bundled with `tembra-contracts` like WASI. When the
kernel creates the root it calls the exported `resources-init.declare` and opens the harbor; when
it creates a `ship` object it calls `service-resources-init.declare` with the service's interface
name, opens the mooring for that object, after the harbor, and closes it when the object goes,
before the harbor. The value lives in the component, one per object; `with` asks the kernel for
the handle of the running object's resource and reaches the value through it. The executor serves
the import itself: the installer requires and grants nothing.

The resources log what they go through with `context::log`, from their `#[open]` and `#[close]`
functions: the kernel runs those outside any call, in a context of the resource's owner, so
`context::object()` in the mooring's `#[open]` is the ship's address, and the record is the Orb's,
as a component and natively.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-service-resource/harbor/Cargo.toml
```

The summary names the resources next to the service:

```text
exports, the services of harbor:
  harbor::example:harbor/ship@1.0.0  methods: berth, load
kernel-managed resources: the kernel opens and closes the resources of this Orb's root and service objects (tembra:guest/resources@1.0.0)
kernel-served context: the call's principal and cancellation, the object's address and the log (tembra:guest/context@1.0.0)
imports: none; the installer grants nothing
```

## Run

```sh
cargo run --manifest-path examples/rust-orb-service-resource/host/Cargo.toml -- examples/rust-orb-service-resource/harbor/artifacts/harbor.orb.wasm
cargo run --manifest-path examples/rust-orb-service-resource/host/Cargo.toml -- --native
```

Both print:

```text
  log: open harbor
  log: open mooring at berth 1 for ship one
ship one moored
  log: open mooring at berth 2 for ship two
ship two moored
berth(one) -> 1   berth(two) -> 2
load(one, 3) -> 3   load(one, 2) -> 5   load(two, 1) -> 1
ship three -> unavailable/resource: the resource `mooring` did not open: every berth is taken
  log: close mooring at berth 1
released one
load(two, 1) -> 2
  log: open mooring at berth 3 for ship three
ship three moored
berth(three) -> 3
  log: close mooring at berth 2
  log: close mooring at berth 3
  log: close harbor
```

The `log:` lines come from the resources' `#[open]` and `#[close]` functions, through the kernel's
context; the host prints each record as the engine, or natively the host, hands it over. The harbor
opens once, with the first ship; each ship's mooring opens when the ship is created, naming the
ship, and closes when the ship is released, giving its berth back; the third ship is refused while
both berths are taken, and moors at a fresh berth once a ship left. Closing the host closes the
remaining moorings first and the harbor last. Natively, the macro's `orb()` declares the harbor on
the Orb and a mooring on each ship object, in the same order, as
[the macro guide](../../crates/orb-macro/README.md#using-the-orb-natively) describes; the host
links the harbor crate and installs `port::orb()` where it loads the artifact otherwise.

## Limits

A service resource belongs to one object and is reached by the methods of that object's service
and the `#[open]` and `#[close]` functions of the object's later resources: a root resource
cannot depend on it, nor can a resource of another service, whose objects come and go on their
own. Resource names are unique in the Orb, since the kernel and `depends` name a resource by its
struct's name.
