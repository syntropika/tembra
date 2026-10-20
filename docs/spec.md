# Runtime behavior

This reference describes Tembra's object, state, execution and transport guarantees. Read the
[guide](guide.md) for the application model or the [tutorial](tutorial.md) for a runnable Rust example.
Public declarations document operation-specific errors and defaults.

## One core per host

A host owns object identity, reference claims, contexts, resources, state and execution versions.
Rust providers, the native V8 executor and browser JavaScript bindings use this Rust core. A host
belongs to one execution thread or event loop; its local futures need not implement `Send`.

Bindings translate values and carry checked handles. Executors run implementations; loaders accept
executable artifacts. Scheduling, transport, compilation, package resolution and file watching
belong to adapters or external tools.

## Contracts and identity

An object's address consists of its contract name, normalized input identity and principal key.
Input normalization applies schema defaults, cleaning and validation before computing canonical
identity. Equal addresses resolve to the same logical object while it remains managed by the host.
Disposal and later acquisition can create a new instance.

A contract version and fingerprint describe binding requirements; they do not create another object
address. Providers must cover their consumers' declared members and types. A different contract
version needs an explicit compatible adapter; placement changes cannot be bridged by that adapter.

Local Rust methods use a checked native channel and can carry owned nonserializable values.
Portable local methods and remote contracts use portable values and runtime schema checks. Remote
contracts validate arguments and results even when the caller shares the provider's host. Opaque
native capabilities are not remote contract members.

The portable profile limits canonical JSON to 1 MiB and nesting to 128 containers. Numbers must be
finite, and integral values must fit the shared safe integer range
`-9_007_199_254_740_991..=9_007_199_254_740_991`. Rust state uses this profile even when its contract
is local. An absent method argument or result is distinct from a present JSON `null`.

## Declaration and readiness

Factories run synchronously. They declare behavior, state, dependencies and resources before
activation; they do not await dependencies or perform asynchronous initialization themselves.
Declared dependencies and resources participate in activation. Initializers and exposed state
hydration must complete before a reference becomes live. Independent activation work may overlap.

`host.get(...).await` returns a ready reference or an error. A failed or dropped acquisition releases
its own claim; other claims and completed external effects remain independent. Failed activation
unwinds managed dependencies and resources. Reacquisition does not overlap a still-disposing object
at the same address.

## References and cleanup

Each acquisition owns a claim. Cloning a generated Rust reference shares that claim; explicit
`reference.core().release()` releases it for every clone. A separately acquired reference has its
own claim. Final drop releases the claim and schedules cleanup, without synchronously joining it.

Referenced objects are disposed after their final claim, subject to configured linger. Host-lifetime
objects remain pinned by the host. Readables and subscriptions are weak views and do not keep an
object or reference claim alive. A retained snapshot keeps its immutable value independently.

Ordinary release can leave an admitted invocation physically active. Forced disposal also makes
references gone and requests cancellation. Managed cleanup retains actual pending callbacks and
resources until their work settles. Resources close in dependency order, and every close is attempted
even if another close fails. Arbitrary application work still requires cooperative termination.

Await `host.close()` to join managed shutdown and observe cleanup errors. The native runner does this
after its callback succeeds or returns an error. It reports a callback error ahead of a simultaneous
cleanup error and attaches the cleanup failure to its details. Dropping or panicking out of work
does not establish graceful shutdown or rollback.

## Contexts and execution versions

A context identifies a host, root, execution version, principal and cancellation scope. A generated
call without an explicit context starts a root. Calls through `reference.with_context(&ctx)` retain
the admitted root and execution version. Expired or foreign contexts are refused rather than
silently becoming new roots. Cloning a context does not extend its scope lifetime.

Object execution uses the object's principal; trusted transport and acquisition boundaries determine
which principals may be admitted. Cancellation ends logical permission to continue and signals
cooperative work. It does not undo external effects or prove that a callback has physically stopped.

## Atomic state

A cell mutation prepares a private candidate and recomputes its affected internal derivations.
Validation succeeds before the candidate values and revisions are adopted together. A callback,
conversion, schema or internal derivation error leaves authoritative values and revisions unchanged
and publishes none of that candidate. This is not an unrelated multicell transaction.

Observers run after adoption. Their errors do not undo committed state. A projection of an external
source cannot roll back a change already committed by that source. External effects in mutation
callbacks or custom serializers are outside the state transaction.

Snapshots are immutable. Typed value reads return decoded owned values; modifying one does not
change the core. State events distinguish snapshots, checked deltas and resets. A delta carries its
expected base and resulting revision; a missing base requires a reset.

## Facets, hooks and replacement

Facets install provider declarations and compatibility requirements. Initial batch installation does
not have the same rollback guarantee as coordinated replacement. Registries expose immutable tables
per execution version. Hooks are explicit pipelines over managed operations and registry entries;
they do not intercept every method automatically.

Native facets remain fixed for the process lifetime. Compatible JavaScript replacement retains
logical objects, surviving references and compatible state. Already admitted roots keep their old
implementation version while new roots use the committed version. State schema changes require
explicit state versions and supported migrations.

Reload preparation precedes one local commit without asynchronous suspension. A refused candidate
leaves the installed implementation and authoritative state in place; cleanup or restoration of
external resources can itself fail and is reported. There is no promise of universal external
rollback. The core bounds current and previous execution versions; retiring old work can request
cancellation before its physical cleanup completes.

## Remote calls and state

Connection logic checks protocol, contract shape, placement and authenticated principal admission.
A remote reference is ready only after its shape and required initial state are hydrated. A returned
method result and state-stream delivery are independent: receiving the result does not prove that
the caller has observed every resulting state update.

A disconnected call may already have executed. Its outcome is unknown and the runtime does not
automatically retry it. Reconnection reacquires references; it does not establish exactly-once
effects, durable state or preservation of the old provider instance. Distributed replacement uses
bounded coordination, without a durable distributed commit guarantee.

## Verification sources

The observable behaviors above are exercised by [core lifecycle and integration tests](../crates/core/tests),
[typed API and state tests](../crates/tembra/tests), [native transport tests](../crates/native/tests)
and [JavaScript executor tests](../crates/executor-js/tests). `just check-all` runs the maintained
native, TypeScript and WASM compilation checks; it does not launch a browser.
