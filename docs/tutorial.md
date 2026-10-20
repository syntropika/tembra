# Counter tutorial

Run a small Rust service, call it through a typed reference and read its managed state.
This walkthrough follows the maintained [entrypoint](../examples/rust/src/main.rs) and
[counter implementation](../examples/rust/src/counter.rs). The Rust snippets below are excerpts
from those files, rather than separate programs.

## 1. Run the example

Install Git, Rustup, just 1.21 or later and Bun 1.4.3. You also need a native C/C++ toolchain.
The repository's [toolchain file](../rust-toolchain.toml) selects Rust 1.99.0, rustfmt, Clippy
and the WebAssembly target.

```sh
git clone https://github.com/syntropika/tembra.git
cd tembra
just setup
just install-ts
just run-rust-example
```

The program checks its results, prints `Count: 1` and awaits managed shutdown. Build output may
appear before that line. See the [justfile](../justfile) for the commands behind these recipes.

## 2. Describe the counter

The contract declares what callers can observe and invoke:

```rust
#[tembra::contract(local, name = "example.counter", version = 1)]
pub(crate) trait Counter {
    /// Observes the managed count, initially zero.
    #[state]
    fn count(&self) -> u32;

    /// Returns the current count or the core's scope/state error.
    async fn read(&self) -> Result<u32>;

    /// Adds one and returns the new count.
    ///
    /// # Errors
    /// Returns `invalid/counter_overflow` at `u32::MAX` without changing state,
    /// or the core's scope/state error.
    async fn increment(&self) -> Result<u32>;
}
```

The macro generates the `Counter` token, `CounterInput`, a typed caller reference and the
`CounterProvider` trait. `local` selects a Rust contract whose method values stay native.
The `#[state]` member gives callers a readable view of the managed count.

## 3. Keep mutation inside the provider

`CounterService` stores a private `Cell<u32>`. It implements `CounterProvider`: `read` reads the
cell, `count` exposes its readable view and `increment` changes it using the call's `Context`.
Callers do not receive the writable cell.

The implementation checks the next value before assigning it:

```rust
    async fn increment(&self, context: Context) -> Result<u32> {
        self.count.change(&context, |count| {
            let updated = count.checked_add(1).ok_or_else(|| {
                RuntimeError::invalid("counter_overflow", "counter reached its maximum value")
            })?;
            *count = updated;
            Ok(())
        })?;
        self.count.value()
    }
```

At `u32::MAX`, this returns an `Invalid` error with reason `counter_overflow` and leaves the
count unchanged. The regression test in the counter source checks both the method result and
the readable state after that refusal. Scope and state failures can also propagate through `?`.

## 4. Register the provider

The example's `serve` helper registers the factory:

```rust
pub(crate) fn serve(host: &Host) -> Result<()> {
    host.serve(Counter, |_, _, declaration| {
        Ok(CounterService {
            count: declaration.state("count", 0_u32)?,
        })
    })
}
```

`host.serve` connects the contract token to a provider factory. The factory uses the declaration
to create the managed `count` state with initial value zero. Registration does not acquire a
caller reference; the next step does that.

## 5. Acquire, call and observe

Inside the native runner's callback, the entrypoint registers the provider and acquires a ready
typed reference. This contract has no input fields, so acquisition uses `CounterInput {}`:

```rust
        counter::serve(&host)?;
        let counter = host.get(Counter, CounterInput {}).await?;
        assert_eq!(counter.read().await?, 0);
        assert_eq!(counter.increment().await?, 1);
        assert_eq!(counter.count().value()?, 1);
        println!("Count: {}", counter.read().await?);
        counter.core().release();
```

`read` returns zero before the increment. `increment` returns one, and the state accessor reads
the same updated value. Each `?` propagates an error instead of treating acquisition, invocation
or state access as infallible.

The typed reference retains a managed claim. Cloning that reference shares the claim; explicitly
releasing it ends the claim for every clone. A `Readable<u32>` obtained from `count()` is weak:
keeping or cloning it does not keep the managed object alive. Read it while the object remains
available and handle state-access errors.

## 6. Finish the host's work

The entrypoint calls `tembra::native::run("example", (), |host| async move { ... })` from a
synchronous `main`. The example enables the facade's optional `native` feature in its
[manifest](../examples/rust/Cargo.toml).

Call this runner outside an already running Tokio runtime; nesting it inside one panics.
The runner keeps the host and callback on one thread and awaits `host.close()` after the
callback returns, including when it returns an error. Releasing a reference alone does not
establish that asynchronous cleanup has finished.

If the callback and cleanup both fail, the callback error is returned with cleanup details.
Panicking or dropping user work does not promise graceful shutdown or rollback of external
effects.

Continue with the [guide](guide.md) for other entry points and the [specification](spec.md) for
the runtime's behavioral model.
