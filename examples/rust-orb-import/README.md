# Rust Orb importing another Orb's interface

This example runs two compiled Orbs. `greeter` is the greeter of
[rust-orb-service](../rust-orb-service/README.md) reduced to one method, `hello`. `welcomer` imports
the greeter's interface and calls `hello` once per name: a compiled Orb calling another Orb, through
the kernel. The host is wiring only: it shows that loading the welcomer without naming a provider is
refused, then binds the import to `greeter` and calls the welcomer.

| File                             | Role                                                                                         |
| -------------------------------- | -------------------------------------------------------------------------------------------- |
| `greeter/src/lib.rs`             | The provider: one service with `hello`; it knows nothing about its callers                   |
| `greeter/contract/greeter.wit`   | The contract `cargo tembra build` wrote, read by the welcomer's build                        |
| `welcomer/src/lib.rs`            | The consumer: `import = "..."` on the attribute, `greeting::hello(name)` in a method         |
| `welcomer/contract/welcomer.wit` | The consumer's contract: the import in the world, the greeter's package nested whole         |
| `host/src/main.rs`               | The wiring: a load refused, then `Install::requires`, then one call; it defines no Orb       |
| `run.sh`                         | Build the greeter, then the welcomer against its contract, show both contracts, run the host |

## The import

```rust
#[tembra::orb(
    package = "example:welcomer@1.0.0",
    wit = "../greeter/contract",
    import = "example:greeter/greeting@1.0.0"
)]
mod welcomer {
    #[service]
    pub struct Welcoming;

    impl Welcoming {
        /// Greet each name through the imported interface and return the greetings.
        pub fn welcome(names: Vec<String>) -> Vec<String> {
            names.iter().map(|name| greeting::hello(name)).collect()
        }
    }
}
```

`import` names an interface by its identity and `wit` the directory holding its package, here the
contract the greeter's own build wrote, so the greeter is built first. The macro adds
`import example:greeter/greeting@1.0.0` to the world and re-exports the generated module as
`greeting`: `greeting::hello(name)` is an ordinary Rust function for the author, with the signature
`wit-bindgen` gives the WIT, `&str` in and `String` out, and a call through the kernel at run time,
under the principal of the call running `welcome`. Nothing in the welcomer names the Orb that
answers.

```wit
world welcomer {
  import example:greeter/greeting@1.0.0;
  export welcoming;
}
```

The greeter's package follows, nested whole, as `cargo tembra build` writes every consumed package:
at installation the kernel compares the provider's contract with this copy.

## The installer binds the import

```rust
engine.load(&welcomer, Install::default())?; // refused: the import has no provider
engine.load(&welcomer, Install::default().requires("example:greeter/greeting@1.0.0", "greeter", 1))?;
```

An import is a hole the installer fills. `requires` names the interface, the Orb that provides it
and the API version; the kernel checks that the named Orb exports that interface with the signature
the welcomer was built against, and refuses the load otherwise. The import reaches the provider's
`main` object. The provider could be native as well: the kernel binds by contract, as
[rust-orb-shared-interface](../rust-orb-shared-interface/README.md) shows with a native keeper of a
WIT interface.

## Build

Run from the repository root; the welcomer is built against the greeter's contract, so the greeter
comes first.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-import/greeter/Cargo.toml
cargo tembra build --manifest-path examples/rust-orb-import/welcomer/Cargo.toml
```

The welcomer's summary lists the import under `imports of other Orbs, bound with Install::requires`.

## Run

```sh
cargo run --manifest-path examples/rust-orb-import/host/Cargo.toml -- examples/rust-orb-import/greeter/artifacts/greeter.orb.wasm examples/rust-orb-import/welcomer/artifacts/welcomer.orb.wasm
```

```text
load without requires -> refused: invalid/wit_binding: example:greeter/greeting@1.0.0: missing Orb provider requirement
welcome(["Ada", "Grace"]) -> ["Hello, Ada!","Hello, Grace!"]
```

The first line is the refusal: the welcomer's world imports an interface and `Install::default()`
names no provider for it. Then one call to `welcome` becomes two calls to the greeter, one per
name, and the greetings come back as a `list<string>`. `run.sh` runs the steps in order.

## Limits

A compiled import reaches the provider's `main` object; another object of the provider is reached
by key with `import("example:greeter/greeting@1.0.0", keyed)`, see
[importing by key](../../crates/orb-macro/README.md#importing-another-orbs-interface-by-key). A
WASI interface is imported with `import = "wasi:..."` and granted instead of bound; see
[importing WASI interfaces](../../crates/orb-macro/README.md#importing-wasi-interfaces).
Handing out an object for the caller to keep and call repeatedly is
a WIT resource; see [rust-orb-wit-resource](../rust-orb-wit-resource/README.md).
