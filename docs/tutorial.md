# Counter tutorial

Run a small Rust service, call it through a typed reference and read its managed state.
This walkthrough follows the maintained [entrypoint](../examples/rust/src/main.rs) and
[counter implementation](../examples/rust/src/counter.rs). The Rust snippets below are excerpts
from those files, rather than separate programs.

## 1. Run the example

Install Git, Rustup and a native C/C++ toolchain. Install just 1.21 or later if you want to use
the repository recipes. This Rust example does not require Node.js, Bun or JavaScript packages.
The repository's [toolchain file](../rust-toolchain.toml) selects Rust 1.99.0, rustfmt, Clippy
and the WebAssembly target.

```sh
git clone https://github.com/syntropika/tembra.git
cd tembra
cargo run --locked -p tembra-example-rust
```

The program increments its count, prints `Count: 1` and awaits managed shutdown. Build output may
appear before that line. `just run-rust-example` runs the same Cargo command. Rustup selects
the checked-in toolchain automatically; the broader `just setup` recipe installs development
tools and Git hooks that are unnecessary for this walkthrough.

## 2. Describe the counter

The contract declares what callers can observe and invoke:

```rust
#[tembra::contract(local, name = "example.counter", version = 1)]
pub(crate) trait Counter {
    #[state]
    fn count(&self) -> u32;

    /// Adds one, refusing overflow without changing the count.
    async fn increment(&self) -> Result<u32>;
}
```

The macro generates the `Counter` token, `CounterInput`, a typed caller reference and the
`CounterProvider` trait. `local` selects a Rust contract whose method values stay native.
The `#[state]` member gives callers a readable view of the managed count.

## 3. Keep mutation inside the provider

`CounterService` stores a private `Cell<u32>`. It implements `CounterProvider`: `count` exposes
its readable view and `increment` changes it using the call's `Context`.
Callers do not receive the writable cell.

The implementation checks the next value before assigning it:

```rust
async fn increment(&self, context: Context) -> Result<u32> {
    self.count.change(&context, |count| {
        *count = count.checked_add(1).ok_or_else(|| {
            RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
        })?;
        Ok(())
    })?;
    self.count.value()
}
```

At `u32::MAX`, this returns an `Invalid` error with reason `counter_overflow` and leaves the
count unchanged. The [counter integration tests](../testing/rust/tests/examples/counter.rs) use
the actual example provider to check its method result and readable state after that refusal.
Scope and state failures can also propagate through `?`.

## 4. Register the provider

The entrypoint registers a factory that creates the example provider with an initial count of zero:

```rust
host.serve(Counter, |_, _, declaration| {
    CounterService::new(declaration, 0)
})?;
```

`host.serve` connects the contract token to a provider factory. The factory uses the declaration
to create the managed `count` state with initial value zero. Registration does not acquire a
caller reference; the next step does that.

## 5. Acquire, call and observe

Inside the native runner's callback, the entrypoint registers the provider and acquires a ready
typed reference. This contract has no input fields, so acquisition uses `CounterInput {}`:

```rust
let counter = host.get(Counter, CounterInput {}).await?;
counter.increment().await?;
println!("Count: {}", counter.count().value()?);
drop(counter);
```

`increment` changes the count to one, and the state accessor reads that committed value. Each `?`
propagates an error instead of treating acquisition, invocation or state access as infallible.

The typed reference retains a managed claim. Cloning that reference shares the claim; dropping
the final clone releases it. A `Readable<u32>` obtained from `count()` is weak:
keeping or cloning it does not keep the managed object alive. Read it while the object remains
available and handle state-access errors.

## 6. Finish the host's work

The entrypoint calls `tembra::native::run("counter-example", (), |host| async move { ... })` from a
synchronous `main`. The example enables the facade's optional `native` feature in its
[manifest](../examples/rust/Cargo.toml).

Call this runner outside an already running Tokio runtime; nesting it inside one panics.
The runner keeps the host and callback on one thread and awaits `host.close()` after the
callback returns, including when it returns an error. Releasing a reference alone does not
establish that asynchronous cleanup has finished.

If the callback and cleanup both fail, the callback error is returned with cleanup details.
Panicking or dropping user work does not promise graceful shutdown or rollback of external
effects.

## 7. Try one additional concept

The other maintained Rust examples use the same public typed API. Each introduces one concept:

| Example | What it adds | Command |
| --- | --- | --- |
| [Native values](../examples/rust/basics/native-values.rs) | Pass an `Rc<Cell<u32>>` through a local typed method, preserving identity. | `just run-example local` |
| [State](../examples/rust/basics/state.rs) | Derive a value, observe changes and retain an immutable snapshot. | `just run-example state` |
| [Facets](../examples/rust/facets/install.rs) | Group typed provider registrations and install the facet. | `just run-example facets` |

Without just, run `cargo run --locked -p tembra-example-rust --example local`, replacing `local`
with `state` or `facets` as needed. These examples do not require assembling core descriptors,
handlers or execution adapters.

Continue with the [guide](guide.md) for TypeScript Hosts and platform loaders, the
[architecture](architecture.md) for module responsibilities, and the [specification](spec.md)
for the runtime's behavioral model.
