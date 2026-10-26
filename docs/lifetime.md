# Resources and tasks

A **resource** is a managed value with open and close callbacks.
A **task** is asynchronous work owned by a scope.
Both live with the host, orb or object where they are declared.

## Resources

Factories declare resources synchronously.
The engine opens them during activation.
Use a resource value only after it is ready.
Dependencies open before their consumers and remain available while those consumers close.

In the SDK, `host.resource(name, options)` declares a host resource.
`self.resource(name, options)` declares a resource in the current scope.
`self.resource(name)` reads a named host resource.
Rust uses `Declaration::resource()` or `resource_async()` for object resources.

An exclusive resource cannot overlap its replacement.
The engine closes the old value before it opens the new value.
Resource reuse depends on its name, version, exclusivity and dependencies.
See [updates](updates.md) for replacement and recovery.

## Tasks and cancellation

In the SDK, `self.task(callback)` starts work after activation.
Its task scope provides a cancellation signal and `root()` for managed operations.

Cancellation requests a stop. It does not prove that work has stopped.
A callback, task or resource opener keeps its ownership until it finishes.
`host.close()` waits for that physical completion and cleanup.
An uncooperative callback can therefore delay shutdown.
Context frames follow their creating call and physical consumers; see [context lifetime](context.md).

## Object lifetime and time

`Lifetime.Referenced` releases an idle object after its claims and active work allow cleanup.
Its optional `linger` value retains the object and tasks for a fixed idle period.
Reacquiring it during that period preserves its state and resources.
`Lifetime.Host` keeps the object until host shutdown and does not use linger.

The host supplies monotonic elapsed time for linger and deadlines.
Clocks on different hosts do not need the same origin.
The native wrapper drives deadlines with its host clock. Embeddings can supply a custom
`Clock`; explicit kernel `tick()` advances remain supported and resynchronize the native origin.
SDK hosts arrange their own timer updates.

See the [lifetime example](../examples/rust/lifetime.rs), [core tasks](../crates/core/src/host/tasks.rs)
and [SDK activities](../packages/ts/sdk/src/runtime/activities.ts).
