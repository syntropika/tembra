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

### Portable hook contracts

An isolated hook uses a versioned WIT interface with one callback and one input parameter.
The hook provider exports that interface. A contributor imports and exports the same interface:
the import declares the required callback contract, and the export implements the contribution.
Tembra derives the callback types, signature and hook requirement from that contract. Hook
callbacks do not become ordinary services.

For example, the `example:hooks/guard@1.0.0` interface can export
`apply: func(input: admission) -> admission`. A source provider declares
`hooks: { guard: waterfall() }`, where `guard` is its generated JavaScript alias. Its factory
binds `self.hook<guard.Admission>("guard")` and invokes `handle.run(context, input)`.
An isolated contributor declares:

```ts
import { orb, type Context, type Declaration } from "@syntropika/tembra-sdk";
import type { guard } from "./contract.wit";

function create(self: Declaration) {
  return { counts: self.resource("counts", { open: () => ({ admitted: 0 }) }) };
}
type Root = ReturnType<typeof create>;

export default orb({
  name: "guard",
  apiVersion: 1,
  create,
  contributes: {
    "example:hooks/guard@1.0.0": {
      priority: 0,
      handle(root: Root, context: Context, input: guard.Admission): guard.Admission {
        context.check();
        root.counts.value.admitted += 1;
        return { value: input.value + 2, blocked: true };
      },
    },
  },
});
```

A source contribution runs in its Orb's root environment. Like a service method, it receives
that environment first (the value returned by `orb.create`), then the issued context and the
hook input. Annotate the environment with its actual type. Managed state belongs to one service
object, so root-level data that contributions share lives in root resources; a service reaches
the same environment through `self.orb`. Native SDK contributions keep `handle(context, input)`.

Installation selects the hook's provider through
`requires: { "example:hooks/guard@1.0.0": { orb: "tools", apiVersion: 1 } }`.
The required hook and its signature are derived automatically. Installation and provider
replacement reject a mismatched signature before activation.

Compiled contributors implement the same WIT callback export. Their build target supplies
`contributes: { "example:hooks/guard@1.0.0": 0 }` alongside `entry`, `wit`, `world` and
`apiVersion`. Only the binding and priority are stored in the artifact; callback slots and
signatures are derived on loading. A Rust contributor can also mark a `pub fn` with
`#[hook("example:hooks/guard@1.0.0", priority = 0)]` under `#[orb]` and build with `cargo tembra`; see the
[Rust hook contribution example](../examples/rust-orb-hook-contribution/README.md). The contributed import is a type anchor, not a callable
service dependency; attempting to call it traps the contributor.

Native Rust providers attach the prepared callback digest with `Hook::signature(...)`.
Matching native Rust participants keep direct typed calls. Values crossing an isolated heap
are encoded and checked against WIT. Both use the same participant ordering and lifetime rules.

Source providers currently support `series`, `waterfall` and `bail`. Their WIT callbacks return,
respectively, no value, the input type, or an option:

- `series` runs every participant in plan order; the hook returns no value, which consumers of a
  portable service observe as `null`.
- `waterfall` passes each result to the next participant.
- `bail` stops at the first participant returning `some`. A WIT `none` (a source `undefined`)
  is no decision and continues to the next participant; if nobody decides, the hook returns
  `none`. A native participant can still decide after isolated ones.

Compiled Orbs can contribute to hooks and declare them, with `series`, `waterfall` or `bail`
composition, when their artifact carries the role: `cargo tembra build` packages it from
`#[hook(..., composition = c)]`, see the
[Rust hook declaration example](../examples/rust-orb-hook-declaration/README.md). Portable middleware, custom composition and observation
are not exposed. Native providers can still apply their own composition, including
`waterfall_until`, to isolated contributions.

The complete source example is in the
[hook fixtures](../packages/ts/sdk/test/fixtures/wasm-hooks/provider.ts), with the
[canonical contract](../crates/executor-wasm/tests/wit/hooks/hooks.wit). The
[hook qualification test](../crates/executor-wasm/tests/hooks.rs) runs waterfall, series and bail
with a real Rust contributor ([crate](../crates/executor-wasm/rust-contributor/src/lib.rs)),
a source contributor and a native participant, under native and source providers on JIT and
interpreted engines. The [compiled contribution test](../crates/executor-wasm/tests/compiled_hooks.rs)
keeps self-contained component cases.

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
