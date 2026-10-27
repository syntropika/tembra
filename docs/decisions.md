# Implementation decisions

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

## Native byte RPC server

The optional `tembra/rpc` feature exposes the existing SDK JSON operations over an injected
synchronous byte sender. The embedding owns authentication, the fixed principal, transport I/O,
queue byte limits and actual transport closure. Tembra adds no socket dependency, retry loop,
identity scheme or lifecycle driver.

`Options::new(principal, allow)` distinguishes catalog visibility (`None`) from acquisition of an
explicit key (`Some(key)`). Neither implies the other. Catalog methods reflect the declared native
access policy; acquisition advertises the actual kernel claim's declared, required and view-allowed
methods through `tembra_core::Host::reference_methods`. Invocation still performs the normal core checks.
Views remain optional: applications relying on service access policies must enable `views`.

Each connection owns its references, observations, request admission and call contexts. Observations
attach before the acquisition reply; updates during reply delivery are retained until that reply is
accepted by the sender. Release detaches observation. Cancellation and closure signal existing roots;
physical callbacks retain their existing managed pins until settlement. There is no effect rollback.

Frame encoding stops at the configured byte limit. Oversized acquisitions are refused before
retaining a remote claim; oversized results return `exhausted/frame_size` when that error fits.
Already performed callback effects remain performed. A live oversized State or sender failure closes
only its connection with a cause. The sender must bound queued and in-flight bytes. These limits do
not bound application value construction, total allocations or buffers owned by another library.

Logical close must wake an idle transport pump. `Connection::closed()` supports one embedding waiter;
`close_cause()` exposes the first terminal cause. The embedding selects closure alongside its I/O and
closes its real channel. Explicit close has no error cause; the first close wins.

A sender must not block the host executor. The stdio example reserves queued and in-flight output
bytes, uses a nonblocking bounded queue, and moves blocking writes onto a separate writer. Writer
failure wakes the pump without new peer traffic; terminal closure is returned after host cleanup.

The SDK's optional `Transport.close(cause?: unknown)` receives `Channel.close`'s terminal cause after
receive/socket listeners detach. An embedding's ownership notification must therefore live in its
close implementation as well as its listener. Nonfatal State observer errors still use `onError`
and do not signal terminal connection loss. Platform close methods require a wrapper that handles
or ignores the cause; transport detachment/close errors cannot prevent terminal request cleanup.
Reconnect and view reacquisition belong to consumers. Aborted acquisitions retain pending-budget
entries until their late reply or closure; late successful claims are released instead of discarded.

The SDK installs an acquired State mirror synchronously while routing the get response, before
resolving its caller. A transport may deliver the reply and subsequent State frames in the same
JavaScript turn. Waiting for the acquisition promise before registering the mirror loses those
updates. This internal attachment correction changes no wire fields or public methods and retains
release, cancellation and connection checks. Portable native observations cache the exact kernel
snapshot value identity so repeated publication of the same snapshot does not duplicate events.
Detaching an observation or releasing its reference immediately removes inactive weak observer
records and empty indexes from the host observer index, including when the State owner remains live
without publishing again. Per-reference observation records remain until that reference is released.
This keeps repeated acquisition/closure bookkeeping bounded while preserving physical call pins.
