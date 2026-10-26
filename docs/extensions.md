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
The native Rust wrapper exposes neither SDK declaration helper; the core provides the lower-level contracts.

See [core hooks](../crates/core/src/hooks.rs), [core subscriptions](../crates/core/src/subscriptions/mod.rs)
and [SDK composition helpers](../packages/ts/sdk/src/authoring/hooks.ts).
