# Calls and context

A **root** is one operation across related calls.
It keeps a principal and an execution version.
A **call** is one admitted method invocation inside that root.
Admission means that the engine accepted the call after its lifetime and access checks.

A **context** carries the root and, inside a callback, the admitted call.
Pass it to dependent calls to keep the operation connected.
State writes require a live call context for the object that owns the state.
Keeping a context object does not extend its lifetime.

## Typed metadata

Use a stable key name, version and value kind.
Rust and JS check the same key contract without generated code.

Inside an SDK root callback:

```ts
const Trace = ContextKey.text("request/trace", 1);
const tagged = ctx.with(Trace, "request-42");
await reference.increment(tagged);
console.log(tagged.value(Trace));
```

Import `ContextKey` from `@tembra/sdk`.
`with()` derives a new context. It keeps the principal, execution version and validity period.
The original context and sibling branches keep their own values.
Calls and hook contributions inherit the frame supplied by their caller.

Kinds are text, safe signed integer, boolean and owner-issued opaque reference.
Storage can issue an opaque reference to pending writes.
Storage validates and closes that reference; Tembra carries it.
A closed reference stays invalid even if its context is still alive.
Copying a JS opaque token does not retain its source frame.
Create a receiving binding while that source lives, or have its owner issue a root-owned source.
A source returned from a completed call has expired unless another owner still retains its frame.
In Rust, an owner can use `host.context(ctx.root, None)` to issue a separate root-owned binding.
The method context still expires when its invocation finishes.

## Cost and release

Each derivation adds one parent-linked frame in Rust.
Propagation sends its ID in the existing call request; it does not copy every value.
A lookup visits at most 64 frames.
Call-created frames are released after their creator and admitted consumers physically finish.
Child frames retain their ancestors until they are released.
Root-created frames stay until the root ends and its physical calls finish.

Limits are 4096 live frames per root and 1024 key contracts per host.
Key names can use 256 bytes; text values can use 4096 bytes.
Sequential calls can reuse the live-frame budget. Root-created derivations still accumulate.
Reuse a derivation when many calls need the same metadata.

Across hosts, `exportValues([Key])` selects scalar metadata explicitly.
Infrastructure must send and restore it. Local opaque references cannot be exported.

See [Rust frames](../crates/core/src/context_values/frames.rs) and [SDK context](../packages/ts/sdk/src/runtime/context.ts).
