# Runtime contract guarantees

## Typed service acquisition

Typed service acquisition uses the named kernel acquisition path on local Host, Context and
Declaration entrypoints. RemoteHost acquires services by name. Source and compiled Wasm Orbs
use WIT interface identities and checked method signatures.

Original and captured service objects resolve identically, including after repeated capture during
installation. A compatible replacement using fresh service definitions preserves existing reference
and State behavior through the normal kernel reload path. No stale implementation is called through
the service object. The object supplies identity and types, not direct handler access.

Typed references describe the declared interface, not a grant of every method. Principal access
views and declared dependency method restrictions remain runtime checks. Root-owned references
are released at root settlement. Explicit release and host cleanup settle physical ownership.
Unassociated definitions fail with `invalid/service` before acquisition. Orb declaration refuses
reuse of the same service object under a different orb, service name or API version with
`invalid/service`, even when consumers acquire only by string. Use fresh service definitions for
those distinct identities. A typed dependency also refuses an alias that resolves to a different
service name; string dependencies resolve their declared aliases. Reference-owned members
and `then` are excluded from the inferred business methods because the proxy reserves them.

The WeakMap is shared by SDK entrypoints and independently bundled SDK copies in the same JavaScript
realm, using a non-enumerable symbol-keyed metadata slot. Independently bundled copies can therefore
use original service objects through ordinary Host, Context and Declaration acquisition. Serialized
definitions, isolated Wasm orbs and other workers or processes do not preserve those object
identities; use WIT contracts and provider-qualified service names at those boundaries.

## Accumulated waterfall stopping

`Hook::waterfall_until` is a native composition behind the facade's `hooks` feature.
The declaring Orb supplies its stopping predicate. Portable hook composition supports
`series`, `waterfall` and `bail`; see [extensions](extensions.md).

The declaring Rust orb owns the stopping predicate. A TypeScript-declared hook continues to use its
own declared composition.

Native tests cover accumulated values, initial terminal input, empty plans, ordering, participant
errors, typed identity and context. These tests do not establish browser, mobile or cross-host
behavior for this native composition. Operational evidence and bounded measurements remain
outside the source repository.

## Owner-bound observation

Rust and JavaScript use the same kernel admission, one-shot source state, exact scope ownership,
versioned plan and cleanup indexes. JavaScript does not implement another lifecycle policy.
Observation is available only through a declared hook handle.

For a root-only source, JavaScript returns `forbidden/observation_source` through the shared
`Errors.Forbidden.ObservationSource` descriptor; Rust returns `invalid/context`. Both reject
without creating an observation or consuming admission.

Observation refuses retired/gated owners rather than substituting a newer plan. A live declaring
scope is required; host or owner cleanup can win admission and prevent observation delivery. Empty
metadata frames do not carry storage transactions. Storage owners still validate explicitly passed
tokens, including closed or foreign-root tokens. Observation errors remain consumer-reported.

Dependent calls under an observation additionally validate exact registered orb/object scopes and
their registered ancestors. Missing scopes return `gone/scope`; closing scopes or ancestors return
`conflict/scope_closed`. These checks ensure every admitted observation call has physical scope claims that cleanup can cancel and await.

No general structured task tree, cross-host finalization or forced callback termination is claimed.

## Native RPC byte adapter

The `rpc` feature exposes `tembra::rpc::{serve, Options, Connection}` for the
SDK JSON `describe`, `get`, `call`, `release`, `cancel` and State envelopes. Native serde methods use
tuple arguments and `()` for no arguments. Tagged BigInt/byte values use their portable JSON representation. A newly acquired claim advertises the kernel's effective method intersection;
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
The adapter does not reconnect automatically.

Native acceptance is in `crates/tembra/tests/rpc.rs`; actual SDK/native byte conformance is run by
`just test-native-rpc`. These tests qualify the native JSON adapter. Browser transport authentication and custom codecs
require their own acceptance tests.
