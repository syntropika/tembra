# Hook decisions

## Accumulated terminal decisions

`Hook::waterfall_until` combines accumulated transformations with early stopping behind the
`hooks` feature. The declaring orb supplies a synchronous predicate over the accumulated result.
The predicate checks the initial input and each successful participant result. The first terminal
value returns unchanged without invoking another participant. An empty plan returns its input.

Participant errors remain errors and prevent later participants from running. Earlier effects are
not rolled back. Applications represent ordinary policy decisions in their result type; they do not
need to turn a terminal decision into an error or preserve it in every contributor.

The implementation uses the existing versioned plan, managed invocation and contribution paths.
Ascending priority and registration order, native typed values, context propagation and portable
decoding retain their existing contracts. Ordinary `waterfall` and `bail` remain unchanged.

This composition is a Tembra extension. The immutable Chord reference is commit
`e17e2e10556cea6b674bb627726bef8ceed90065` in
[earendil-works/pi](https://github.com/earendil-works/pi/tree/e17e2e10556cea6b674bb627726bef8ceed90065/packages/chord).
Its inspected source and tests do not define this hook composition. Tembra's acceptance depends on
executed native and adapter tests rather than an upstream parity claim.

## Owner-bound observation

Owner-bound observation is a hook operation owned by its original physical invocation. It is not an
arbitrary callback capability on Context. Rust uses `HookHandle<A, ()>::observe(source,
payload)`; JavaScript uses the bound hook's `observe(source, payload)`. Both use one shared
kernel admission and the existing hook plan and execution adapters.

### Admission and authority

The source must be a physically retained invocation in the same host and declaring orb. An
object-bound hook additionally requires that exact source object. Orb-bound hooks retain the
source object when called from an object invocation. A bare root, foreign owner, settled source,
observation context or observation descendant cannot admit observation. A physical source accepts
at most one successful observation admission across its hook handles. Validation refusal does not
consume admission; successful admission consumes it even for an empty plan or participant failure.
The consumed marker ends with the source invocation, whose absence independently rejects reuse.

Observation preserves the original principal and exact execution version. A fresh ordinary root
starts with an empty metadata frame; trace data belongs in the explicit typed payload. Original
contexts remain cancelled or expired, and old frames or opaque storage access are never revived.
Normal version gates, owner closure and host shutdown still apply. The physical execution owner
awaits observation before returning; a logically cancelled caller need not wait for physical work.
Observer errors remain separate from the application's primary result and grant no effect replay.

### Physical ownership and cleanup

The kernel records exact original-owner scope anchors for the observation root. These remain until
its final physical descendant settles and the root ends. Each active dependent call temporarily
claims its own exact scopes; settlement releases those claims. Completed dependencies do not
retain or cancel the remaining observation. Historical root memberships remain solely the existing
version/gate history and are not queried for observation cleanup.

Admitting a dependent call requires its exact registered owner scopes and their registered
ancestors to exist and remain open. Missing or closing scopes refuse that call before retaining it,
even when the corresponding ordinary call would otherwise be admitted. Ordinary calls retain their
existing admission rules.

An index from exact scope to observation roots supports cancellation and cleanup waiting. An empty
index makes ordinary cleanup independent of ordinary root population. Scope cancellation targets
only live observation claims. Closing a source scope or a scope with an active dependent call
cancels observation and waits for physical settlement; closing a completed dependency does neither.
Host close rejects new admissions and cancels all ordinary roots using its existing policy.
Cancellation remains cooperative and cannot prove termination or undo an external effect.

`waterfall_until` is an independent composition and is not required by observation.
