# Hook compatibility

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

Ordinary hook execution and original cancellation checks retain their existing behavior.
Observation refuses retired/gated owners rather than substituting a newer plan. A live declaring
scope is required; host or owner cleanup can win admission and prevent observation delivery. Empty
metadata frames do not carry storage transactions. Storage owners still validate explicitly passed
tokens, including closed or foreign-root tokens. Observation errors remain consumer-reported.

The pinned Chord reference is `e17e2e10556cea6b674bb627726bef8ceed90065`. Its waiter cancellation and
physical promise lifetime were inspected; this owner-bound hook operation is a Tembra extension.
No general structured task tree, cross-host finalization or forced callback termination is claimed.
