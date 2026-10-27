# Compatibility

## Typed service acquisition

Service-object acquisition adds a TypeScript binding convenience over the existing named kernel
acquisition. Orb declarations, string acquisition, RPC messages and native service registration
retain their current form. The convenience is available on local Host, Context and Declaration
entrypoints; RemoteHost remains a dynamic service-name client.

Original and captured service objects resolve identically, including after repeated capture during
installation. A compatible replacement using fresh service definitions preserves existing reference
and State behavior through the normal kernel reload path. No stale implementation is called through
the service object. The object supplies identity and types, not direct handler access.

Typed references describe the declared interface, not a grant of every method. Principal access
views and declared dependency method restrictions remain runtime checks. Root-owned references
are still released at root settlement, and explicit release and host cleanup remain unchanged.
Unassociated definitions fail with `invalid/service` before acquisition. Orb declaration now refuses
reuse of the same service object under a different orb, service name or API version with
`invalid/service`, even when consumers acquire only by string. Use fresh service definitions for
those distinct identities. A typed dependency also refuses an alias that resolves to a different
service name; string dependencies retain their existing alias behavior. Reference-owned members
and `then` are excluded from the inferred business methods because the proxy reserves them.

The WeakMap is shared by SDK entrypoints and standalone orb bundles in the same JavaScript realm,
using a non-enumerable symbol-keyed metadata slot. Independently bundled copies can therefore use
original service objects through ordinary Host, Context and Declaration acquisition. Serialized
definitions and other workers or processes do not preserve those object identities; use the dynamic
service-name interface at those boundaries. No extra cross-process contract validation or
compatibility layer is introduced.

## Accumulated waterfall stopping

`Hook::waterfall_until` is an additive constructor behind the native facade's `hooks` feature.
Existing `series`, `waterfall`, `bail` and ordinary hook execution keep their behavior.

The declaring Rust orb owns the stopping predicate. A mixed plan dispatches JavaScript contributors
through the existing portable adapter, so they require no new SDK dispatcher, registry or result
encoding. A TypeScript-declared hook continues to use its own declared composition.

Native tests cover accumulated values, initial terminal input, empty plans, ordering, participant
errors, typed identity and context. Embedded V8 acceptance runs the same mixed asynchronous case
through standard and cooperative execution, including result/input decoding errors and ordinary
waterfall behavior. These tests do not establish browser, mobile or cross-host compatibility for the
new native composition. Operational evidence and bounded measurements remain outside the source
repository.

## Owner-bound observation

Rust and JavaScript use the same kernel admission, one-shot source state, exact scope ownership,
versioned plan and cleanup indexes. JavaScript does not implement another lifecycle policy.
Observation is available only through a declared hook handle.

For a root-only source, JavaScript returns `forbidden/observation_source` through the shared
`Errors.Forbidden.ObservationSource` descriptor; Rust returns `invalid/context`. Both reject
without creating an observation or consuming admission.

Ordinary hook execution and original cancellation checks retain their existing behavior.
Observation refuses retired/gated owners rather than substituting a newer plan. A live declaring
scope is required; host or owner cleanup can win admission and prevent observation delivery. Empty
metadata frames do not carry storage transactions. Storage owners still validate explicitly passed
tokens, including closed or foreign-root tokens. Observation errors remain consumer-reported.

Dependent calls under an observation additionally validate exact registered orb/object scopes and
their registered ancestors. Missing scopes return `gone/scope`; closing scopes or ancestors return
`conflict/scope_closed`. Ordinary calls keep their existing admission rules. This stricter check
ensures every admitted observation call has physical scope claims that cleanup can cancel and await.

The pinned Chord reference is `e17e2e10556cea6b674bb627726bef8ceed90065`. Its waiter cancellation and
physical promise lifetime were inspected; this owner-bound hook operation is a Tembra extension.
No general structured task tree, cross-host finalization or forced callback termination is claimed.

## Native RPC byte adapter

The `rpc` feature adds `tembra::rpc::{serve, Options, Connection}` without changing the existing
SDK JSON `describe`, `get`, `call`, `release`, `cancel` or State envelopes. Native serde methods use
tuple arguments and `()` for no arguments. Existing tagged BigInt/byte values retain their portable
JSON representation. A newly acquired claim advertises the kernel's effective method intersection;
forged requests remain subject to normal core invocation checks.

The server has separate frame, pending-request and reference limits. An initial oversized State or
method result is a structured request refusal if that refusal fits. An oversized live State closes
with `exhausted/frame_size`; applications can inspect the cause and stop reacquiring that view.
A tiny frame budget that cannot encode the error also closes. Sender refusal is terminal and is
never retried. Reference release and logical close do not prove physical callback settlement.

Every SDK channel close supplies its terminal cause after listener detachment. Transports must
handle or ignore that argument. Wrap platform close methods: forwarding the cause directly to
`WebSocket.close(code)`, `destroy(error)` or `end` can throw or change their behavior. A zero-argument
implementation remains suitable only when it ignores extra arguments. Detachment, close and error
reporter failures cannot skip the channel's terminal cleanup. Observer reporting remains
nonterminal. Acquired State mirrors attach during response routing so same-turn State frames are
retained before the acquisition promise resumes. Native observation detachment also removes stale
weak entries from the host observer index immediately, so quiet live owners do not retain released
reference subscriptions. Per-reference observation records remain until that reference is released.
Aborted acquisitions retain bounded pending tombstones until a late response or connection close.
A late successful acquisition is released; a reply that cannot create a usable proxy is also released
when it carries a valid reference. Tombstones and cleanup requests share the pending-request limit.
No reconnect behavior or new error hierarchy is introduced.

Native acceptance is in `crates/tembra/tests/rpc.rs`; actual SDK/native byte conformance is run by
`just test-native-rpc`. These tests qualify this JSON adapter, not the pinned reference product's
separate versioned wire protocol, browser transport authentication or an arbitrary custom codec.
