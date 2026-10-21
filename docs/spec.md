# Runtime behavior

This reference describes Tembra's object, state, execution and transport guarantees. Read the
[guide](guide.md) for the application model or the [tutorial](tutorial.md) for a runnable Rust example.
The [architecture](architecture.md) maps these guarantees to their owners. Public declarations
document operation-specific errors and defaults. The rules here describe implemented behavior;
they do not imply that every language facade exposes every advanced core primitive.

## One core per host

A host owns object identity, reference claims, contexts, resources, state and execution versions.
Rust providers, Node-API and browser JavaScript bindings, and the optional embedded V8 executor
use this Rust core. A host belongs to one execution thread or event loop; its local futures need not
implement `Send`.

Node's JavaScript callbacks execute in the existing Node runtime, connected through Node-API.
Browser callbacks execute in their browser realm, connected through `wasm-bindgen`. The optional
embedded V8 executor is a separate Rust integration, not a dependency of the Node binding.

Bindings translate values and carry checked handles. Executors run implementations; loaders prepare
facets. Scheduling, transport, compilation, package resolution and file watching belong to adapters
or external tools.

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

Portable unions can use different object cleaning and defaults when every branch requires a
shared property with a distinct string `const` and no default on that property. The literal tag
selects the branch before cleaning; its value remains part of object identity. Unknown or missing
tags are refused for object values. Branches without that proof still require identical
normalization policies. Nullable wrappers preserve explicit nested null values.

Typed Rust portable decoding normalizes safe integral JavaScript numbers before deserializing
integer fields, including values beyond the JavaScript engine's small-integer representation.
Fractional and unsafe integer values are refused rather than truncated. Generated portable
providers and consumers use the same decoder; native-only Rust method values remain native.

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
TypeScript references expose `release()` explicitly. `hold()` returns an acquired reference before
readiness; `get()` awaits that readiness and releases its claim if readiness fails.

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

Facet and managed-task roots also retain protected local execution origin. A receiver's
`Context::caller_facet()` reports the immediate managed caller's installed Facet name; nested calls
report the immediate receiver Facet, not an earlier caller. Direct Host roots and RPC ingress are
unattributed, regardless of principal overrides. This observation does not replace scope admission,
remote authentication, or the embedding's trust in native code. Compatible replacement preserves
the stable name. Calls that omit an explicit Context start an unattributed Host root.

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

Rust's `host.installed_facets()` reads the actual installed run catalog and returns owned name/origin
observations sorted in case-sensitive Rust `str` order. Initial synchronous setup failure withdraws
its scoped run; eager activation failure leaves its admitted declaration in the catalog. A reentrant
query during initial setup can see that setup's scoped run. Reload candidates stay absent through
candidate setup and activation until commit, which can precede resolution of the reload waiter.
Completed Host shutdown empties the catalog even when it reports cleanup errors. Presence means
catalog admission, not readiness, startup success or package provenance. Absence does not establish
that old calls stopped or resources were physically released. Snapshot values retain no executable
callbacks or resources; equality compares name and origin only, so same-name replacement can produce
equal observations without identifying a code version or generation.

Native facets remain fixed for their Host lifetime. Compatible JavaScript replacement retains
logical objects, surviving references and compatible state. Already admitted roots keep their old
implementation version while new roots use the committed version. State schema changes require
explicit state versions and supported migrations.

Reload preparation precedes one local commit without asynchronous suspension. A refused candidate
leaves the installed implementation and authoritative state in place; cleanup or restoration of
external resources can itself fail and is reported. There is no promise of universal external
rollback. The core bounds current and previous execution versions; retiring old work can request
cancellation before its physical cleanup completes.

Bindings retain generation callbacks and values while Rust execution owners or physical invocations
still need them. A public Promise settling or an awaiter losing interest does not establish physical
completion. This also applies to detached hook terminals. A successful reload can publish the new
version while old physical work is still retiring; it does not promise that old modules have been
collected when the reload Promise resolves.

## Platform loading and execution profiles

The shared TypeScript operations are `install`, `load`, `reload`, `get`, `hold` and `close`.
`install` accepts a callback facet. `load` accepts a module string/URL or `EsmArtifactV1` plus an
optional selected export, defaulting to `default`. `reload` accepts a batch of dynamic module,
artifact or callback facet inputs and uses the existing Rust replacement mechanism.

Module acquisition and evaluation produce a checked facet definition. Its registration, compatibility,
resources and execution ownership then belong to the platform bridge and core. No artifact envelope
is required for ordinary platform modules. The loader's execution profile remains observable:

- **Node source (`node-source-v1`)** uses pinned Jiti 2.7.0 transformed evaluation. Local source
  paths and file URLs have no query or fragment; entries must be `.ts`, `.mts`, `.cts`, `.js` or
  `.mjs`. A preparation owns a fresh Jiti graph with module and filesystem caching disabled. The
  profile is synchronous transformed evaluation, with CommonJS wrapper semantics rather than native
  ESM equivalence. `require`, `module`, `exports`, `__filename`, `__dirname` and the wrapper's
  `arguments` are observable. Top-level await, JSX and `tsconfig` aliases are unsupported. Installed
  packages may resolve; no dependency installation is performed. Builtins and native packages retain normal
  Node caching. Compiler bootstrap is isolated from application Error globals in a compiler-only
  VM context; facet code runs in the main Node realm. This is not an untrusted-code sandbox.
- **Browser modules (`browser-module-v1`)** use native ESM import. Browser URL resolution, import
  maps, CSP, CORS and dependency caches apply. The loader does not automatically version the URL or
  invalidate transitive modules. Loading the same URL again may return its cached namespace.
- **Closed artifacts (`tembra-esm-v1`)** retain their format, source hash, export and closed ESM
  validation. External imports and dynamic dependency resolution are refused. Node and browser
  use native ESM evaluation for this profile, not the Node source transformer. The optional Rust
  executor uses its embedded engine for this explicit delivery format.

These profiles are trusted execution environments. A source profile does not silently weaken a
closed artifact's validation or redefine its semantics. TypeScript transformation does not establish
type safety; use the tooling typechecker separately.

### Browser public interface

The browser Host owns its binding wrappers and executor together. Public integration uses
`createHost`, including explicit initialized WASM modules or asset initialization, and the Host's
acquisition, context, connection, facet and diagnostic operations. Callers do not separately free
internal WASM handles or create another executor over the same Host.

The public TypeScript consumer `Readable<T>` exposes a current value and value subscriptions.
Raw consumer revision snapshots, delta/reset subscriptions and per-member hydration waits are not
supported public operations. These former raw WASM capabilities were removed; they are not implied
by the unified facade. Reference readiness remains supported. Producer cells still expose their
declared snapshot operations. The [Browser interface decision](architecture.md#browser-public-interface)
records the rationale and the conditions for adding a narrower advanced interface.

## Module admission and memory

Retiring a Tembra code generation means releasing its managed registrations, callbacks, values and
execution owners after required work settles. Physically unloading a JavaScript module means the
runtime no longer retains its code and associated module graph. These are distinct events. Neither
native ESM cache behavior nor releasing Jiti's managed references guarantees physical collection.
Revoking a browser Blob URL also does not establish module collection.

Admission is cumulative and finite in the execution environment, not refunded by reload retirement,
Host close or construction of another Host in that environment. Evaluation already submitted to the
runtime remains charged even if evaluation or the later core commit fails. Invalid input refused
before submission does not consume an evaluation. Concurrent preparation is refused rather than
allowing reservations to race. Current limits are:

| Profile | Evaluations | Cumulative admitted source | Additional limits |
| --- | --- | --- | --- |
| Node source | 256 source entries | 64 MiB of source admitted to transformation | 4,096 transformed modules; 2 MiB per module. |
| Node closed artifact | 32 | 8 MiB | 1 MiB per artifact. |
| Browser module | 256 root import attempts | Not observable | One native module import in flight; transitive bytes/modules are unknown. |
| Browser closed artifact | 32 | 8 MiB | 1 MiB per artifact; realm and Rust-session admission both apply. |

Node source byte accounting measures UTF-8 source submitted to its transform, including charged
transitive transforms; it is not emitted JavaScript size or live heap occupancy. Native package
allocations and application effects are outside this accounting. Finite evaluation admission limits
SDK-managed repeated loading; it is not a hard memory cap on arbitrary trusted code.

Node emits a warning when a source or artifact budget reaches 75 percent. Source and browser-module
diagnostics expose warning thresholds and exhaustion indicators. Budget refusals carry a stable
reason and dimension/usage details where applicable. `host.diagnostics()` exposes current generation
ownership and platform admission, and remains available as a final snapshot after close. Node process
memory fields describe the process, not memory exclusively owned by a particular facet.

When admission is exhausted, the application must explicitly rotate the relevant process or realm
to admit fresh evaluations. The SDK never resets a Host silently. HostManagers supervise independent
child Hosts through Node child processes or browser Workers; `replace()` shuts down an environment
and creates another. It is not `reload()`, reference preservation, state migration or durable recovery.

## HostManager liveness and retention

`start()` waits for both the child bootstrap and the Rust RPC connection to become ready.
Afterward, `instance.status()` reports the observed lifecycle state. Core connection closure or a
platform process/Worker failure changes that state to `failed` and starts teardown. `instance.closed`
observes completion of the manager's teardown attempt; it does not prove that every resource closer
succeeded. `instance.failure()` preserves the first causal terminal failure, including a structured
core reason when available. A normal stop leaves this diagnostic undefined; a failed stop can leave
the instance stopped with a recorded error and reject the stop operation.

Rust RPC owns ping/pong and silence detection. Managed links use its current default 10-second ping
interval and 30-second liveness deadline; there is no separate JavaScript heartbeat. Timers require
the supervising scheduler to progress, so `ready` is not an instantaneous health guarantee during
event-loop blockage or browser suspension. Activity counts include protocol/control traffic; neither
those counts nor `lastMessageAt` measures application demand or proves peer health.

A healthy started environment remains retained even with zero application references. Explicit
`stop()`, `replace()`, parent shutdown or detected failure ends that retention. There is no idle-stop,
automatic restart or configurable `keepAlive` switch. Node's managed IPC and frame pump keep its
parent event loop referenced; this interface does not provide an `unref` policy. Object lifetime and
consumer linger remain separate core policies. Applications that need an idle policy decide when
to call `stop()` using their own demand information, rather than inferring demand from RPC traffic.
Stopping a child does not close its parent Host or release that Host's own scheduler work. Full
application shutdown still requires awaiting the parent Host's `close()`.

## Portable hooks and local capabilities

TypeScript portable hooks use generic binding adapters and the Rust hook manager. Hook arguments,
results, strategy composition and issued invocation scopes remain checked. An arbitrary JavaScript
function cannot substitute for an issued managed hook operation or retain a valid `next` capability
after its invocation scope ends.

JavaScript opaque capabilities require explicit registration of an exact nominal native identity.
Providers expose them through issued, object-owned resources. Forged structural handles, unknown
identities and cross-owner handles are refused. A live local reference can observe the original
JavaScript value identity; releasing the reference removes its authority to read that member.
The capability does not become portable data or a generic adapter for arbitrary Rust native types.

## Errors and cancellation boundaries

Rust operations return typed runtime errors with a category, stable reason and optional details.
Binding errors preserve machine-readable runtime information; SDK declaration validation can also
throw `ContractError` with a reason and value path. Callers must not assume every thrown JavaScript
value is a `ContractError` or every failure occurs before external effects.

Cancellation is a cooperative lifecycle signal, not forced interruption of arbitrary JavaScript,
Rust or external I/O. Managed resources and callback leases remain owned until the underlying work
physically settles. A callback that never cooperates or settles can prevent graceful shutdown;
an application may terminate an isolated environment explicitly, with the corresponding loss of
its in-memory Host state.

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

The observable behaviors above are exercised by [core lifecycle and integration tests](../testing/rust/tests/core),
[typed API and state tests](../testing/rust/tests/tembra), [native transport tests](../testing/rust/tests/transport.rs)
and [JavaScript executor tests](../testing/rust/tests/executor-js). `just check-all` runs the maintained
native, TypeScript and WASM compilation checks; it does not launch a browser. TypeScript integration
and ownership tests live under [testing/typescript](../testing/typescript). `just test-integration`
runs the centralized Rust, TypeScript and actual browser integrations; the browser runner requires
its configured Chromium automation tooling. Library unit tests remain in their language packages.
