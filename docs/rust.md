# Rust

Use the `tembra` crate to declare and run native orbs.
It shares the ownership engine from `tembra-core`.

## Declare an orb

```rust
use tembra::{Orb, Service};

fn greeting() -> Orb {
    let service = Service::new("greeting", |_| Ok(()))
        .method("hello", |_, _, name: String| async move {
            Ok(format!("Hello, {name}"))
        });
    Orb::new("greeting", 1).service(service)
}
```

`Service::new()` receives a synchronous declaration factory.
It can declare state and managed resources.
`method()` binds one callback for local and portable calls.
Its argument type must implement `DeserializeOwned`; its result must implement `Serialize`.
Local invocation still passes native values without serializing them.

## Execute and close

The application supplies a `LocalSpawn` executor, such as `LocalPool::spawner()`.
`Host::new()` does not start a runtime or worker thread.
Use `host.install()` to register orbs and `host.get()` to acquire an object reference.
Native `get()` returns before activation finishes; `ready().await` waits for it.

Use `host.root()` to obtain a context.
`reference.call(&ctx, method, arguments)` waits for activation and invokes the method.
Calls check their argument and result types at native dispatch.
Release the reference, then run `host.close()` to completion on the executor.

## Examples

Run these from the repository root:

| Command                                            | Source                                                                    |
| -------------------------------------------------- | ------------------------------------------------------------------------- |
| `cargo run --locked -p tembra --example counter`   | [orb.rs](../examples/rust/orb.rs) and [host.rs](../examples/rust/host.rs) |
| `cargo run --locked -p tembra --example addresses` | [Shared and separate objects](../examples/rust/addresses.rs)              |
| `cargo run --locked -p tembra --example lifetime`  | [Resource reuse and cleanup](../examples/rust/lifetime.rs)                |
| `cargo run --locked -p tembra --example executor`  | [Application executor](../examples/rust/executor.rs)                      |

`just examples` runs all four.
For embedded JavaScript, use `tembra-executor-js::Runtime` with the built SDK adapter.
That runtime supports calls between JS and installed native services through the shared kernel.

See [native declarations](../crates/tembra/src/orb.rs) and [V8 execution](../crates/executor-js/src/runtime.rs).
