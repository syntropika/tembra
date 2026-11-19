# Rust Orbs linked into a native host

This example links two Orb crates into one native binary. `english` and `italian` implement the
same interface, `example:greeter/greeting@1.0.0`, of a WIT package they share. Each is an ordinary
`#[tembra::orb]` crate, which `cargo tembra build` compiles into a component; the host depends on
both as Cargo dependencies instead and installs the Orb each module's `orb()` builds: the same
code, run natively, without an artifact or a WebAssembly engine.

| File                           | Role                                                                                                |
| ------------------------------ | --------------------------------------------------------------------------------------------------- |
| `wit/greeter.wit`              | The shared contract: the `greeting` interface with `hello`                                          |
| `english/src/lib.rs`           | One greeter: `#[service("example:greeter/greeting@1.0.0")]`, in English                             |
| `italian/src/lib.rs`           | The other greeter of the same interface, in Italian, naming the facade `sdk`                        |
| `english/contract/english.wit` | The contract `cargo tembra build` wrote, the shared package nested; `italian/` has its own          |
| `host/src/main.rs`             | The native host: depends on both crates, installs `english::orb()` and `italian::orb()`, calls both |
| `run.sh`                       | Build both crates as components, show their contracts, then build and run the native host           |

## One crate, two targets

```rust
#[tembra::orb(package = "example:english@1.0.0", wit = "../wit")]
pub mod english {
    pub mod greeter {
        #[service("example:greeter/greeting@1.0.0")]
        pub struct Greeting;

        impl Greeting {
            pub fn hello(name: String) -> String {
                format!("Hello, {name}!")
            }
        }
    }
}
```

The author does not choose a target. In a component build, the bindings the macro generates define
the component's exports, symbols named after the interface, such as
`example:greeter/greeting@1.0.0#hello`. Two crates exporting one interface would define the same
symbols, which one native binary cannot link, so the macro defines them only for `wasm32`: a native
build of the crate has none. A native host gets the module's `orb()` instead, which exists only
where the `tembra` facade links the runtime. The greeters depend on `tembra` with `macros` only,
`italian` under another name, `sdk`, which its attribute gives with `crate = "sdk"`; the host
depends on it with its default `host` feature, and Cargo unifies the features of one build, so the
greeters' facade links the runtime too when the host compiles them, whatever name each gives it.

## The host

```rust
host.install(vec![english::orb()?, italian::orb()?])?;
let main = |orb: &str| {
    let service = format!("{orb}::example:greeter/greeting@1.0.0");
    host.get(&service, vec!["main".into()], "alice").map(Rc::new)
};
let in_english = english::greeter::GreetingClient::new(main("english")?);
// Under a root context.
let hello: String = in_english.hello(&context, "Ada".into()).await?;
```

`orb()` builds the Orb that `[package.metadata.tembra]` of its crate names, `english` at API
version 1, with one service per `#[service]`, named `<orb>::<interface identity>` as for a
component and carrying the signature of the shared interface: the kernel checks both greeters
against one contract. `GreetingClient` is the typed client the macro generates for each service,
in the service's module: each method takes the call's context and then the function's parameters, so the compiler checks
the call, and the arguments move as Rust values, without JSON.

## Build

Run from the repository root.

```sh
cargo install --path crates/cargo-tembra
cargo tembra build --manifest-path examples/rust-orb-native/english/Cargo.toml
cargo tembra build --manifest-path examples/rust-orb-native/italian/Cargo.toml
```

```text
exports, the services of english:
  english::example:greeter/greeting@1.0.0  methods: hello
imports: none; the installer grants nothing
```

The host does not read the components: `run.sh` builds them to show that the crates it links
natively are components too, with the services the native Orbs declare.

## Run

```sh
cargo run --manifest-path examples/rust-orb-native/host/Cargo.toml
```

```text
english::example:greeter/greeting@1.0.0  hello("Ada") -> Hello, Ada!
italian::example:greeter/greeting@1.0.0  hello("Ada") -> Ciao, Ada!
```

The host compiles both greeters as its dependencies and links them into one binary. `run.sh` runs
the steps in order.

## Limits

A module that uses a feature the native side of the macro does not cover yet still builds into a
component, and fails to compile natively with an error naming the item; see
[using the Orb natively](../../crates/orb-macro/README.md#using-the-orb-natively). A host that
installs one crate twice, or names the Orb at run time, calls `orb_named(name, api_version)`
instead of `orb()`.
