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
`local_method()` accepts types without those portable traits and refuses every portable call
before entering the callback; use it for host-owned values such as streaming HTTP bodies.

`orb.declaration()` borrows its `core::Orb` metadata before installation. The `services`
list exposes each service's `name`, `methods` and optional canonical `signature`, so consumers
can check a required service and compare its declared signature with the expected contract.
Inspection does not install the Orb, execute factories or call methods; callbacks remain private.
Read descriptor values through an ordinary method call after installation. `api_version` is
the API contract version and remains separate from the consumer's distribution version.

## Execute and close

The application supplies a `LocalSpawn` executor, such as `LocalPool::spawner()`.
`Host::new()` does not start a runtime or worker thread.
Use `host.install()` to register orbs and `host.get()` to acquire an object reference.
Native `get()` returns before activation finishes; `ready().await` waits for it.

Use `host.root()` to obtain a context.
`reference.call(&ctx, method, arguments)` waits for activation and invokes the method.
Calls check their argument and result types at native dispatch.
Release the reference, then run `host.close()` to completion on the executor.
Native asynchronous callback panics become structured `unavailable` callback errors;
other queued work can continue and shutdown still waits for physical settlement.

Failed host declarations release their scopes and resource names. Orb installation
publishes metadata before running factories, as in JavaScript. A failed orb factory
keeps its original error for later acquisitions; it never supplies a fallback environment.

## Examples

Run these from the repository root:

| Command                                            | Source                                                                    |
| -------------------------------------------------- | ------------------------------------------------------------------------- |
| `cargo run --locked -p tembra --example counter`   | [orb.rs](../examples/rust/orb.rs) and [host.rs](../examples/rust/host.rs) |
| `cargo run --locked -p tembra --example addresses` | [Shared and separate objects](../examples/rust/addresses.rs)              |
| `cargo run --locked -p tembra --example lifetime`  | [Resource reuse and cleanup](../examples/rust/lifetime.rs)                |
| `cargo run --locked -p tembra --example executor`  | [Application executor](../examples/rust/executor.rs)                      |

`just examples` runs these and the hook/composition examples below.

See [native declarations](../crates/tembra/src/orb.rs).

## Isolated orbs

Native Rust orbs are trusted application code. To run TypeScript or a compiled component in
WebAssembly, build it into a `.wasm` artifact and load it with `tembra-executor-wasm`:

```rust,ignore
use tembra_executor_wasm::{Engine, Install, Limits};

let engine = Engine::new(Limits::default())?
    .interpreter(&interpreter_bytes)? // only for TypeScript source orbs
    .filesystem("data", data_directory)?
    .network("outbound")?;
let orb = engine.load(&artifact_bytes, Install::default().requires(import, "provider", 1))?;
host.install(vec![orb])?;
```

`Engine::load` returns an ordinary `Orb`. It installs, reloads and closes like a native orb and can
call or provide services through the same kernel. Each loaded orb runs in its own Wasm Store.
See [Wasm orb execution](../crates/executor-wasm/README.md) and
[the Wasm consumer example](../examples/rust/wasm.rs). To write a compiled orb in Rust without a
WIT file, see [the Rust orb example](../examples/rust-orb-service/README.md): `#[orb]` derives the
contract from a module and `cargo tembra build` packages the artifact. A service of such an orb
can own kernel-managed state with `#[state]`, validated by the state's own rule; see
[the Rust state example](../examples/rust-orb-state/README.md). Such an orb can also declare a hook
for other orbs to contribute to; see
[the Rust hook declaration example](../examples/rust-orb-hook-declaration/README.md). Its methods
can read the call's principal, the object's address and write to the host's log through the
kernel's context interface, see [the Rust context example](../examples/rust-orb-context/README.md),
and each service can choose the lifetime of its objects, see
[the Rust service lifetime example](../examples/rust-orb-service-lifetime/README.md). A service
can implement an interface of a shared WIT package instead of deriving one, see
[the Rust shared interface example](../examples/rust-orb-shared-interface/README.md), import
another orb's interface and call it, see
[the Rust import example](../examples/rust-orb-import/README.md), and hand out WIT resources to
other orbs, see [the Rust WIT resource example](../examples/rust-orb-wit-resource/README.md).

## Serve incoming HTTP

A compiled component can export the standard `wasi:http/handler@0.3.0` interface. The application
owns its listener, authentication and routing, then passes each request to the installed orb:

```rust,ignore
use std::rc::Rc;
use tembra_executor_wasm::ingress::{handler_service, serve};

let reference = Rc::new(host.get(&handler_service("web"), vec!["main".into()], "authenticated-user")?);
reference.ready().await?;
let (response, settled) = serve(&host, "authenticated-user", reference, request).await?;
// Send the status, fields and streaming body with the application's HTTP server.
// The host's supplied executor must keep running while the response is consumed.
send_response(response).await?;
settled.await?;
```

`serve` returns the headers before the body ends. It owns a root on the host's existing executor,
so returning the response or dropping the completion observer does not expire the orb's authority
mid-stream. The body is `Send`; completion and cancellation stay on the host's thread.
`settled.cancel()` cancels the request; dropping its response body signals disconnect and closes
the incoming request body. `host.close()` waits for physical completion.

A handler-only orb needs no outgoing HTTP grant. Importing the outgoing client still requires
its own provider and origin grant. The handler is a host-local entrypoint; other orbs cannot
bind to its owned request/response resources through the portable value ABI.

One streaming request holds the revision's single callback slot. `Limits.timeout_ms` covers the
whole request, including its body; exceeding it quarantines that revision. A guest that ignores
all disconnect signals also remains bounded by that timeout. Source handlers use the same
standard handler contract. The SDK exposes its host-owned request entrypoint separately;
see [WASI coverage](../crates/executor-wasm/README.md#wasi-coverage) for platform qualification.

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
With `views`, `Service::access` applies principal-specific method restrictions to
acquisitions. References expose `status()` and retain normal deterministic `release` and
`Drop` behavior. Policy errors reject acquisition or revoke methods on existing claims.
Enable matching `hooks`, `subscriptions` and `views` features on the native facade.

`Reference::call` moves typed native Rust values without
serializing them and does not require extra serialization traits. `call_portable` always
uses the provider's portable entrypoint, whatever its language or isolation, so a typed
native provider receives the same encoded arguments as a dynamic caller. Its arguments are
serializable values or a `tembra::portable::Payload`. Its results are deserializable values,
which refuse host capabilities, or a type that implements `FromPayload`. It preserves the
original root, frame, principal and code version.

## Owned WIT resources

Compiled Orbs installed with `Engine::load` can return a resource as the complete result and
borrow that resource as a method's implicit `self`. For example, this WIT interface:

```wit
interface sessions {
  resource session {
    constructor(seed: u64);
    add: func(n: u64) -> u64;
  }
}
```

After acquiring the compiled interface as `sessions`, native Rust uses a typed owned handle:

```rust
use futures::FutureExt;
use tembra::portable::Owned;

let session: Owned = host.root("alice", move |context| async move {
    sessions.call_portable(&context, "[constructor]session", (4_u64,)).await
}.boxed_local()).await?;

let total: u64 = host.root("alice", move |context| async move {
    let total = session.call_portable(&context, "[method]session.add", (2_u64,)).await?;
    drop(session);
    Ok(total)
}.boxed_local()).await?;
```

Each construction creates a separate guest instance. `Owned` preserves the originating
reference's API, method and view restrictions; it can outlive that reference and its root.
Method calls still enter ordinary kernel admission. Its implicit borrow stays physically
pinned until the call settles, even if the caller abandons its result. Dropping `Owned`
requests destruction on the issuing Store's worker. Handles are neither cloneable nor
serializable, and a numeric value cannot stand in for one.

For decoder or session identity, declare a typed resource factory such as
`open-keyed: func(key: tuple<string, string>, seed: u64) -> own<session>` and keep that key in
the resource's state. The caller chooses it on each open; a resource method can return it.
Equal keys still create independent instances. These keys do not select or deduplicate kernel
objects: plain compiled WIT imports still bind the provider's `main` object. Each method keeps
the originating claim's admission restrictions and uses the current call's root authority.
To select kernel objects by key, use a [typed reference view](tooling.md#keyed-dependency-references).
Its resource is a local handle to a root-owned service claim, with different ownership from
these provider-created resources.

One compiled revision owns one Store and serves one callback at a time, across all its
sessions. A suspended method on one session therefore queues a call on another session of that
revision. Guest drops are released after the importing callback settles; dropping a resource
inside a long callback does not synchronously run its destructor in the provider.

A guest resource belongs to its exporter revision. Reload does not recreate it in the new
Store; stale methods return `Gone/resource`. Environment release and host close drain the
issuing Store's remaining handles. `Engine::resources()` counts entries still held in those
tables; `Engine::instances()` counts live Stores. A removed entry may still be completing
its destructor.

This transport currently covers compiled exporters with native or compiled callers through
the Rust facade. Source Orbs and the Node/mobile JSON ports refuse inter-Orb resource contracts until
their capability carriers are implemented. General resource arguments, nested resource
results and futures remain outside this resource profile. Byte streams have a separate linear
attachment described below.

## Portable byte streams

A compiled or source Orb installed with `Engine::load` can return bytes to a native Rust
caller without buffering the full body, when its async freestanding export's complete WIT
result is `stream<u8>`:

```rust,ignore
use futures::{FutureExt, StreamExt};
use tembra::portable::ByteStream;

host.root("alice", move |context| async move {
    let mut stream: ByteStream = frames
        .call_portable(&context, "frames", (64_u32, 16_384_u32, 0_u32))
        .await?;
    while let Some(chunk) = stream.next().await {
        consume(chunk?).await?;
    }
    stream.close().await?;
    Ok(())
}.boxed_local()).await?;
```

The end arrives while the producer still runs under its original admitted call. The executor
uses a 64 KiB pipe and applies backpressure. `ByteStream` is neither cloneable nor serializable;
its receiving root owns the right to read it. Ending or cancelling that root closes the end
and refuses further reads. Keep the root alive while consuming it; use `Host::spawn_root` for
a consumer that must outlive its initial caller.

Dropping the end asks the producer to stop once. `close().await` additionally waits for the
producer callback and its admitted Orb-import calls to settle physically. A trusted native
callee that ignores cancellation can keep this close pending until it returns; the guest's
callback deadline does not bound that native work. Hooks and other asynchronous host requests
retain their ordinary abandonment behavior.
The executor reports clean EOF only after the producing call succeeds; a trap after
the last byte is still an error. `Host::close` waits for physical settlement even if the
consumer abandons its result.

Source methods return `writeChunks(chunks)` from `@syntropika/tembra-sdk`, an
`AsyncIterable<number>`, or a `Uint8Array`. They use the existing bounded stream writer;
bytes cross as binary chunks. Through the native Rust facade, source Orbs can also import
these streams from native Rust, compiled components and other source Orbs. The source
consumer uses `readChunks(stream)` from the same SDK, or iterates bytes with `for await`.
It can forward a reader by returning `writeChunks(readChunks(stream))`. A domain stream
does not require a WASI import or grant. The Node, browser and mobile SDK ports still refuse
domain stream contracts until their capability carriers are implemented.

The whole source stream runs within one callback and shares its fuel and wall-time limits.
Expiry quarantines the producing revision. Prefer `writeChunks` over per-byte generation for
large bodies. The revision serves no other callback until production ends. After the reader
closes, the producer's asynchronous iterator cleanup cannot rely on starting new host operations.

Source and compiled Orbs can import that same async `stream<u8>` signature from a compiled,
source or native provider. The host pumps bytes into the importing Store with bounded
backpressure and at most 16 outstanding stream imports per callback, until their readers close
or that callback ends.
A compiled producer contributes at most 64 KiB in its pipe, 64 KiB in the pump's current chunk
and 64 KiB in the importer pipe. A trusted native `ByteSource` controls its own chunk size;
Tembra bounds the importer pipe, but does not split or bound that source's allocation.
Consume an imported reader within the callback
that received it; returning from that callback closes it, even if guest state retains it.
Each revision serves one callback at a time, so consume a stream before opening
another against that same producer revision.

When a source or compiled callback abandons a pending Orb import, the executor cancels that
issued call and normally joins it before the callback settles. Cancellation also reaches its
descendants, without cancelling sibling calls or their root. An import abandoned before its
provider starts need not create a resource at all. If the caller itself is cancelled or the
Store is retired, its callback stops joining; outstanding calls remain signalled and counted
until they finish. `Host::close` still waits for them.

Native Rust callers can control one invocation explicitly with `Reference::issue_portable`
or `Owned::issue_portable`. These return an `IssuedCall` and a future for the logical result.
`IssuedCall::cancel()` signals that invocation and its descendants;
`IssuedCall::settled().await` waits for its physical completion. Inside an entered invocation,
`Context::signal()` observes that call's cancellation as well as its root's cancellation.

A plain WIT `stream<u8>` has no error payload. A compiled consumer sees end/drop on producer
failure; the host retains the cause and quarantines a trapped producer. The number of bytes
delivered before failure can vary with scheduling. EOF alone therefore does not prove success
for an imported stream. Model any required completion result explicitly in the application
contract; this profile does not yet admit general WIT futures.

Stream arguments, nested streams, non-byte elements, resource-method streams and futures are
not admitted. The Node, browser and mobile data ports refuse domain
stream contracts.
WASI HTTP body streams use their standard resources and host transport instead.

## Typed hooks (`hooks`)

Declare a shared `Hook<A, R>` descriptor with `Hook::series`, `Hook::waterfall`,
`Hook::waterfall_until` or `Hook::bail`. Attach it with `Orb::hook`, and bind its handle
during an orb or service factory using `Declaration::hook(&descriptor)`. Only the declaring orb
can bind that hook. Register participants with `Orb::contribute(target, &descriptor, priority, handle)`;
external targets must be declared in `requires`, including the hook name and API version.
Native contracts with incompatible argument/result types are rejected before installation.

| Composition                     | Participant result | Run result                     | Empty plan     |
| ------------------------------- | ------------------ | ------------------------------ | -------------- |
| `Hook::<A, ()>::series`         | `()`               | `()`                           | `()`           |
| `Hook::<A, A>::waterfall`       | `A`                | `A`                            | Original input |
| `Hook::<A, A>::waterfall_until` | `A`                | First terminal or final result | Original input |
| `Hook::<A, Option<R>>::bail`    | `Option<R>`        | First `Some`                   | `None`         |

Plans run in ascending priority, then shared registration order. Series supplies the
same input to each participant; waterfall passes each result forward; bail stops at the
first decision. Series and bail require `A: Clone`; choose `Rc`-backed application data
when shared identity matters. Every composition stops on error.

`waterfall_until(name, should_stop)` checks the initial input and each successful accumulated
result. The first value accepted by the predicate returns unchanged, and later participants do
not run. This lets one contributor transform arguments and a later contributor block the operation
without requiring subsequent contributors to preserve that block:

```rust
use tembra::Hook;

#[derive(serde::Serialize, serde::Deserialize)]
enum Decision {
    Continue { arguments: String },
    Block { reason: String },
}

let before_call = Hook::<Decision, Decision>::waterfall_until("before_call", |decision| {
    matches!(decision, Decision::Block { .. })
});
```

An empty plan returns the input, and terminal input invokes no contributor. Contributor errors
propagate as errors; earlier external effects are not rolled back. Represent an ordinary terminal
decision in the result type. The predicate does not alter cancellation or object ownership.

Local Rust participants receive native values. The kernel selects a plan from the caller's pinned version and admits every contribution
under its own orb. The whole run also retains the declaring scope. Dropping the wait,
releasing a reference or requesting cancellation does not finish an active callback.
Keep the supplied executor running through shutdown.

Run `cargo run --locked -p tembra --features hooks --example hooks` for a complete example.

### Owner-bound observation

A bound `HookHandle<A, ()>` can `observe(&source, payload)` once on behalf of its original physical
invocation. This uses the same plan, ordering and dispatch as ordinary hooks.
The payload is application-defined; Tembra imposes no execution-outcome type. `run` remains the
ordinary repeatable operation. `observe` provides a separate owner-bound admission that remains
possible after the original operation was cancelled or its logical waiter ended.

```rust,ignore
let primary = execute(&ctx).await;
let payload = describe(&primary);
let observer_result = finished.observe(&ctx, payload).await;
report_observer_result(observer_result);
primary
```

The declaring orb must match the source invocation. An object-bound handle also requires its exact
object; an orb-bound handle retains the actual source object. A bare root, settled source, foreign
owner, observer or observer-dependent call cannot issue this admission. One successful admission
consumes the source invocation's allowance across all its hook handles, even for an empty plan or
participant failure. Validation refusal does not consume it. Original calls remain cancelled or
expired. Keep the physical callback alive and await observation before returning; its logically
cancelled caller need not wait for physical completion.

Observation uses the original principal and exact version with an empty metadata frame. Carry
correlation in the payload. Old frames and storage authority are not restored. Closing or retired
owners and version gates can refuse observation; no newer plan is substituted. Consumer policy
keeps observer errors separate from the primary result and never treats them as replay permission.

Exact source-scope anchors retain the original owner through all still-owned physical descendants.
Dependent calls temporarily retain their own scopes. Once a dependent call has finished, releasing
its reference and cleaning its scope do not cancel the remaining observation. Cleanup of the actual
source owner or an active dependent call cancels relevant observation and waits for settlement.
Cancellation is cooperative, so an uncooperative observer can delay cleanup. Historical root visits
remain version/gate bookkeeping and do not define observation ownership.

Rust and the JavaScript bound hook's `observe(source, payload)` use the same kernel admission and
ownership indexes. See [decisions](decisions.md) and [runtime contracts](contracts.md). This interface
adds no arbitrary callback operation on `Context` and no separate consumer lifecycle manager.

Dependent calls inside observation additionally require exact registered orb/object scopes and
their registered ancestors to exist and remain open. A missing scope returns `gone/scope`; a
closing scope or ancestor returns `conflict/scope_closed`. Ordinary calls keep their existing
admission rules. This allows cleanup to account for every admitted observation call.

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
it does not require Tokio. Custom clocks can override `Clock::now` for fractional milliseconds;
`Clock::now_ms` remains the integer deadline interface. Manual kernel ticks resynchronize
the native clock origin, so subsequent native operations never move time backwards.

Run `cargo run --locked -p tembra --example composition` for managed dependencies,
shared orb data and reference-bound selected state.

## Capability boundary

| Capability                                                          | Native facade status                                                                                                                          |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| Typed hooks, including `waterfall_until`                            | Implemented behind `hooks`                                                                                                                    |
| Managed service dependencies and root-owned acquisitions            | Implemented with kernel ownership and readiness edges                                                                                         |
| Root/task/resource cancellation and physical shutdown               | Implemented                                                                                                                                   |
| Typed managed events and bounded queues                             | Implemented behind `subscriptions`                                                                                                            |
| Native state reads, cached selections and reference-bound observers | Implemented; Rust `PartialEq` replaces JavaScript `Object.is`                                                                                 |
| Scope-owned tasks and host/orb/object resources                     | Implemented; typed resource handles provide native injection                                                                                  |
| Acquisition version/method requirements and access policy           | Implemented; access policy requires `views`                                                                                                   |
| Idle/linger and lifecycle deadlines                                 | Driven by the native clock/timer adapter                                                                                                      |
| Foreign service calls preserving an entered context                 | Explicit `call_portable`; local `call` keeps its original trait requirements                                                                  |
| Synchronous Rust views of Wasm-owned state                          | Not exposed; foreign data can be returned through portable methods                                                                            |
| Arbitrary hook policies, including parallel and middleware          | Available to trusted TypeScript orbs; the native authoring surface supplies the four typed compositions above                                 |
| TypeScript and compiled Wasm orbs                                   | Loaded through `tembra-executor-wasm` as ordinary `Orb` values                                                                                |
| Native replacement                                                  | `Host::reload` accepts new native or loaded Wasm declarations; state migration and exclusive replacement resources remain outside this facade |
| RPC, multi-host routing, bundling and loading                       | Separate infrastructure; unchanged by native authoring support                                                                                |

Hook behavior tests live in `crates/tembra/tests/` (`hooks.rs`, `waterfall_until.rs`,
`hook_observation.rs` and `hook_observation_regression.rs`). Other facade tests cover composition,
events, reactive state and clocks. They exercise public interfaces against the actual kernel,
including active work during closure.
