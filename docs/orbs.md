# Orbs and objects

An **orb** is a named, versioned module.
It declares services, dependencies and extension points.
Its code owns business rules and validation.

A **host** installs orbs and manages their shared lifetime.
The application supplies the execution environment.

## Services and objects

A **service** declares a factory and callable methods.
An **object** is one instance of that service.
Its address has three parts:

```text
service + tuple of string keys + principal
```

Equal addresses in the same host share an object.
A different key or principal identifies a different object.
For example, counters for `visits` and `errors` have separate state.

A **principal** is the caller identity supplied by trusted application code.
Tembra carries that identity. The application authenticates it.
An SDK service can use `access(principal)` to restrict its callable methods.
The optional `views` feature enforces method restrictions on individual references.

## References and dependencies

A **reference** is a claim on an object.
In TypeScript, `host.get()` waits for activation; `host.hold()` returns a claim before activation.
Release explicit claims with `release()`. Rust also releases a reference on drop.

In the SDK, `requires` declares the services or hooks an orb consumes.
`self.get()` acquires a dependency owned by the declaring scope.
`ctx.get()` acquires a reference owned by that root operation.
Synchronous factories must wait until later to read a dependency that is still activating.

Generated IDs identify records within one host and are never reused there.
They fit exactly in a JavaScript `Number`. They are not addresses for another host.

To inspect declarations without creating objects, use `installedOrbs()` in TypeScript
or `installed_orbs()` on the Rust kernel.

See the [address example](../examples/rust/addresses.rs) and [core records](../crates/core/src/model.rs).
