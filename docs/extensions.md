# Hooks and events

Hooks extend a named operation.
Event sources deliver values over time.
The engine manages their ownership and admission.
The SDK executes their callbacks.

## Hooks

Declare hooks in an orb's `hooks` field.
A contributing orb lists the hook in `requires` and its callback in `contributes`.
Inside a factory, `self.hook(name)` obtains the handle.
Call `handle.run(ctx, input)` with a live context.

The engine selects a versioned participant plan and orders it by priority.
The hook's `run` function decides how to compose those participants.
Capabilities describe the hook's behavior; they do not replace call or state ownership checks.

| SDK helper             | Behavior                                                          |
| ---------------------- | ----------------------------------------------------------------- |
| `series()`             | Await each participant with the same input.                       |
| `waterfall()`          | Pass each result to the next participant.                         |
| `bail()`               | Return the first result that is not `undefined`.                  |
| `parallel()`           | Await all participants, including remaining work after a failure. |
| `middleware(terminal)` | Let each participant wrap a single-use continuation.              |

The native Rust facade also provides `Hook::waterfall_until(name, should_stop)` to combine
accumulated transformations with first-terminal-value stopping. It checks the initial input and
each successful result. See [typed Rust hooks](rust.md#typed-hooks-hooks) for its contract and
mixed Rust/JavaScript contributions.

### Owner-bound observation

A bound hook can `observe(source, payload)` once on behalf of its original physical invocation,
including after logical cancellation. The source must belong to the declaring orb and any bound
object. The payload is application-defined. Successful admission is single-use across that source's
hook handles, including an empty plan or a participant failure; validation refusal does not consume
it. An observation or its dependent calls cannot open another observation.

Observation preserves the original principal and versioned plan, starts with empty metadata, and
keeps original cancellation effective. Await it inside the physical callback before returning.
Scope cleanup and host shutdown still apply; cancellation is cooperative. Ordinary `run` remains
repeatable under a live context. See the [Rust](rust.md#owner-bound-observation) and
[TypeScript](typescript.md#owner-bound-hook-observation) guides for admission errors, dependent-call
scope checks and physical ownership guarantees.

## Event sources

A source supplies `subscribe(callback)` and returns a detach function.
Declare `self.on(name, source, callback, options)` during a factory call.
Deliveries run with managed contexts under the scope's lifetime.

Rust owns bounded queues and delivery scheduling.
The default SDK queue limit is 64 pending values.
A byte limit also requires a `sizeOf` function.
Queue overflow stops the listener and reports an error.
State-readable sources can replace queued values with their latest value.

`state.subscribe()` observes local state directly.
`self.on()` gives a source managed delivery and lifetime handling.
Neither mechanism is a persistent event log.

Enable `hooks` for managed hooks and `subscriptions` for managed source delivery.
Local state selectors and observers do not need the `subscriptions` feature.
The native Rust facade provides typed hooks and managed events through its own declaration API;
see the [Rust guide](rust.md). The core provides the lower-level contracts.

See [core hooks](../crates/core/src/hooks.rs), [core subscriptions](../crates/core/src/subscriptions/mod.rs)
and [SDK composition helpers](../packages/ts/sdk/src/authoring/hooks.ts).
