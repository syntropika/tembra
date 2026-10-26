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

`just examples` runs these and the hook/composition examples below.
For embedded JavaScript, use `tembra-executor-js::Runtime` with the built SDK adapter.
That runtime supports calls between JS and installed native services through the shared kernel.

See [native declarations](../crates/tembra/src/orb.rs) and [V8 execution](../crates/executor-js/src/runtime.rs).

## Native composition and ownership

`Orb::create` declares one shared typed environment. Service factories read it with
`Declaration::orb::<T>()`; entered hook contributions and task operations can use
`Context::orb::<T>()`. Declare host resources with `Host::declare` before installing
orbs, then inject them with `Declaration::host_resource` or `Context::resource`.
All three scope levels use the same kernel resource programs and physical settlement.
`Host::ready().await` waits for host and orb openers after synchronous installation;
native hook participants also wait for their own orb scope before entering.
`Resource::version` reports the declared contract version.

`Declaration::get(service, key)` resolves the declaring orb's own services or its
explicit `Requirement::methods`, carrying the required API version and method set.
It records ownership and readiness edges before provider activation and rejects cycles.
Scope cleanup releases the claim after resource closers finish; dependency calls from a
cleanup context remain subject to kernel admission. `Context::get` instead returns a
root-owned claim, revoked when the root ends even if its Rust handle is retained.
`Host::get_with_options` exposes independent acquisition with `AcquisitionOptions`.
With `views`, `Service::access` applies principal-specific method restrictions to both
Rust and embedded JavaScript acquisitions. References expose `status()` and retain
normal deterministic `release` and `Drop` behavior.

`Reference::call` retains the existing Rust-only contract and does not require extra
serialization traits. `call_portable` accepts serializable arguments and deserializable
results and can reach JavaScript through the attached executor, preserving the original
root, frame, principal and code version. Use a tuple or vector for a JavaScript argument
list. Both methods bypass serialization when the selected callback is Rust.

## Typed hooks (`hooks`)

Declare a shared `Hook<A, R>` descriptor with `Hook::series`, `Hook::waterfall` or
`Hook::bail`. Attach it with `Orb::hook`, and bind its handle during an orb or service
factory using `Declaration::hook(&descriptor)`. Only the declaring orb can bind that
hook. Register participants with `Orb::contribute(target, &descriptor, priority, handle)`;
external targets must be declared in `requires`, including the hook name and API version.
Native contracts with incompatible argument/result types are rejected before installation.

| Composition                  | Participant result | Run result   | Empty plan     |
| ---------------------------- | ------------------ | ------------ | -------------- |
| `Hook::<A, ()>::series`      | `()`               | `()`         | `()`           |
| `Hook::<A, A>::waterfall`    | `A`                | `A`          | Original input |
| `Hook::<A, Option<R>>::bail` | `Option<R>`        | First `Some` | `None`         |

Plans run in ascending priority, then shared registration order. Series supplies the
same input to each participant; waterfall passes each result forward; bail stops at the
first decision. Series and bail require `A: Clone`; choose `Rc`-backed application data
when shared identity matters. Every composition stops on error.

An omitted JavaScript input is decoded as a Rust unit/null-compatible input.
`None` in bail means JavaScript `undefined`, while `Some(Value::Null)` remains a defined
`null` decision. The existing host codec transports values only at language crossings.
Local Rust participants receive native values, and a plan can contain both languages.
The kernel selects a plan from the caller's pinned version and admits every contribution
under its own orb. The whole run also retains the declaring scope. Dropping the wait,
releasing a reference or requesting cancellation does not finish an active callback.
Keep the supplied executor running through shutdown.

Run `cargo run --locked -p tembra --features hooks --example hooks` for a complete example.

## Cancellation, tasks and events

`Context::signal()` exposes cooperative root cancellation, including shared-adapter
cancellation and host shutdown. Its `is_cancelled`, `check` and `cancelled().await`
operations use the same signal mechanism as `ResourceSignal`. Checking a context still
rejects expired invocation authority independently of cancellation observation.

`Declaration::task` starts a callback after scope activation. `TaskScope::signal`
observes task cancellation; `TaskScope::root` issues a managed owner operation under
the current published version, matching the kernel's task-root policy. Task ownership
continues until physical callback settlement. These are in-memory lifecycle tasks.

With `subscriptions`, adapt a local producer using `EventSource::new`. Its subscription
callback receives an `EventHandler<T>` and returns a detach callback. Register it with
`Declaration::on(name, source, options, handler)`. `EventOptions` defaults to 64 pending
values and can set pending count, an explicit byte-sizing function and byte limit, or
latest-value coalescing. The kernel owns queue admission and payload release. A handler
receives the declaring owner's principal and an admitted invocation. Queue overflow
stops admissions and detaches the source; handler errors are reported through
`Host::errors`. Delivery acknowledgement and resource cleanup wait for physical handler
settlement. Sources must support synchronous detachment, including detachment during a
subscription callback.

## Reactive state and deadlines

`State::readable` and `Reference::state::<T>` expose immutable native snapshots.
`Readable::select` caches projections and uses `PartialEq` to retain equal results.
`subscribe` delivers the current value immediately and returns an `Observation` that
detaches on drop. A reference-bound view becomes invalid and all its observations stop
when the reference is released. Notification failures do not revert adopted state.
The observer handle must be retained for as long as observation is desired.

The native host drives kernel clock ticks and keeps one timer for the next deadline,
including idle/linger expiry. `Host::with_clock` permits a supplied monotonic `Clock`
and wait implementation. The default uses monotonic system time and `futures-timer`;
it does not require Tokio. Embedded JavaScript takes over deadline driving while using
the same clock origin. Construct `TokioExecutor::new()` inside the embedding's running
`LocalSet`; its dispatcher retains that execution context across suspended V8 ops.

Run `cargo run --locked -p tembra --example composition` for managed dependencies,
shared orb data and reference-bound selected state.

## Capability boundary

| Capability                                                                  | Native facade status                                                                                                                                                 |
| --------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Typed series, waterfall and bail hooks; mixed Rust/JS participants          | Implemented behind `hooks`                                                                                                                                           |
| Managed service dependencies and root-owned acquisitions                    | Implemented with kernel ownership and readiness edges                                                                                                                |
| Root/task/resource cancellation and physical shutdown                       | Implemented, including shared V8 action dispatch                                                                                                                     |
| Typed managed events and bounded queues                                     | Implemented behind `subscriptions`                                                                                                                                   |
| Native state reads, cached selections and reference-bound observers         | Implemented; Rust `PartialEq` replaces JavaScript `Object.is`                                                                                                        |
| Scope-owned tasks and host/orb/object resources                             | Implemented; typed resource handles provide native injection                                                                                                         |
| Acquisition version/method requirements and access policy                   | Implemented; access policy requires `views`                                                                                                                          |
| Idle/linger and lifecycle deadlines                                         | Driven by the native clock/timer adapter                                                                                                                             |
| Foreign service calls preserving an entered context                         | Explicit `call_portable`; local `call` keeps its original trait requirements                                                                                         |
| Synchronous Rust views of JavaScript-owned state                            | Not exposed; foreign data can be returned through portable methods                                                                                                   |
| Arbitrary JavaScript hook policies, including parallel and middleware       | Available in JavaScript; the native authoring surface supplies the three requested typed compositions                                                                |
| Native hot replacement, state migration and exclusive replacement resources | Outside this facade; compiled native implementations remain nonreplaceable. An exclusivity boolean alone would not provide the missing replacement/restore lifecycle |
| RPC, multi-host routing, bundling and loading                               | Separate infrastructure; unchanged by native authoring support                                                                                                       |

Behavioral tests live in `crates/tembra/tests/{hooks,composition,events,reactive,clock}.rs`
and `crates/executor-js/tests/{hooks,native_facade}.rs`. They exercise public interfaces
against the actual kernel and V8 runtime, including active work during closure.
