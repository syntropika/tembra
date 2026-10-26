# State

Each object can declare one versioned state value.
Use fields in that value to group related data.
The orb supplies a synchronous validator.
The kernel stores the adopted value ID, revision and state version.

## Change state

Inside an SDK method with a declared `state` handle:

```ts
state.update(ctx, (current) => ({ ...current, count: current.count + 1 }));
```

An update follows these steps:

1. Read the current value and revision.
2. Build and validate a candidate value.
3. Check the call, owner and revision.
4. Adopt the candidate.
5. Notify observers after adoption.

A rejected candidate leaves the current state unchanged.
An old revision cannot overwrite a newer revision.
The state change function must finish synchronously.

## Read and observe

In JS, state contains plain data. The SDK freezes objects and arrays recursively.
Unchanged branches can retain their identity.
`state.value` reads the current snapshot.
An earlier snapshot keeps its earlier value.

`select(compute)` caches a derived value.
`subscribe(callback)` receives the current value, then changes, until detached.
Selectors use `Object.is` to decide whether their result changed.
For an array length, select `items.length`; a different length produces a different number.
Observer failures are reported without undoing an adopted change.

Rust `State<T>` uses checked updates and returns `Rc<T>` snapshots.
The validator belongs to the Rust service.
The Rust wrapper does not expose the SDK's selector and observer helpers.

See [native state](../crates/tembra/src/state.rs), [SDK state](../packages/ts/sdk/src/state/binding.ts)
and [state migration](updates.md).
