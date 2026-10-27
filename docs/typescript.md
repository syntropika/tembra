# TypeScript

The SDK declares JS orbs and runs their callbacks.
Rust checks ownership and lifecycle transitions.
Application values stay in the JS runtime until a language or transport boundary needs a codec.

## Run an orb in Node

```ts
import {
  orb,
  type Context,
  type Declaration,
  type ServiceDefinition,
  type State,
} from "@syntropika/tembra-sdk";
import { createHost } from "@syntropika/tembra-sdk/node";

type Counter = { count: State<number> };

const counterService = {
  create(self: Declaration) {
    return { count: self.state(0, { validate: Number.isSafeInteger }) };
  },
  methods: {
    increment({ count }: Counter, ctx: Context) {
      return count.update(ctx, (value) => value + 1);
    },
  },
} satisfies ServiceDefinition;

const counter = orb({
  name: "counter",
  apiVersion: 1,
  services: { counter: counterService },
});

const host = createHost();
try {
  await host.install(counter);
  const reference = await host.get(counterService, "main");
  console.log(await reference.increment()); // 1
  reference.release();
} finally {
  await host.close();
}
```

Factories finish synchronously. Methods and resource callbacks can be asynchronous.
`host.get()` waits until the object is ready.
A method call without an explicit context starts its own root.
Pass an existing context as the last argument to join that operation.
Returning a context or copying an opaque token does not keep its issuing call alive.
See [context lifetime](context.md) before sharing a binding beyond a method return.

Passing a service definition infers the reference's methods, business arguments and asynchronous
results. The implementation's environment and Context parameters are removed from public calls.
The returned `count` State handle also makes `reference.state.value` a number. A factory may return
the State handle directly or as a property of its environment. If its State is declared only as a
side effect, its type remains unknown; handler environments and opaque resources are not inferred.

`host.hold(service, key)`, `ctx.get(service, key)` and `self.get(service, key)` accept the same
definitions. Held references expose `ready`; Context references end with their root; declared
dependencies still require the owner's existing `requires` and method permissions.

`orb()` associates original service objects and its frozen copies with their registered names and
API versions. Declare the Orb before acquiring a service. One service object must not be reused
under a different orb, name or API version; create a fresh definition for that identity. Compatible
reloads continue to dispatch through current managed references rather than captured handlers.

Passing a service name string still returns a dynamic reference. This supports discovered services,
native implementations and explicit RPC boundaries without importing executable definitions.
SDK entrypoints and independently compiled Orb bundles share the identity WeakMap within the same
JavaScript realm. This also supports typed dependencies declared inside loaded standalone bundles.
The association is not serialized or shared across workers and processes.
Keep runtime validation in the orb even when application code supplies TypeScript types.

## Browser and Worker

Import `createHost` from `@syntropika/tembra-sdk/browser` and await it.
It loads `@syntropika/tembra-engine-wasm` unless the application supplies a kernel or WASM module.
Serve the generated WASM asset with its loader, or set `wasmInput`.
The same entrypoint works in a module Worker.

## React Native

Import `createHost` from `@syntropika/tembra-react-native` for iOS and Android.
It creates a host synchronously through the native TurboModule.
See the [React Native guide](react-native.md) for installation and cleanup.

## Packages

Node uses `@syntropika/tembra-engine-node` and its compiled native addon.
Build the engines for a local checkout; SDK source imports do not compile Rust.
Bun uses the SDK's TypeScript source exports. Node's default ESM exports use built JavaScript.
The root Git package points to SDK sources and declares exact engine package versions.
Git consumers resolve those engines separately; local development uses workspace links.
The root manifest defines the Bun workspaces. The lockfile and tool versions are also at the root.

See [SDK entrypoints](../packages/ts/sdk/package.json), [Node host](../packages/ts/sdk/src/node.ts)
and [browser host](../packages/ts/sdk/src/browser.ts).

## Owner-bound hook observation

A hook bound by `self.hook(name)` exposes `observe(source, payload)` as well as ordinary `run`.
Observation admits one selected hook plan for the original physical owner, including after caller
cancellation. The payload remains application-defined. Keep the primary result separate:

```ts
const primary = await execute(ctx);
let observerError;
try {
  await finished.observe(ctx, describe(primary));
} catch (error) {
  observerError = error;
}
reportObserverError(observerError);
return primary;
```

A successful admission is single-use across that source invocation's bound hooks; an empty plan or
observer failure still consumes it. Refused validation does not consume it. A bare root, settled
source, foreign owner or observer-dependent call cannot admit observation. An object-bound hook
requires its source object. An orb-bound hook preserves the actual source object's ownership.

JavaScript rejects a root-only source with `forbidden/observation_source`, described by
`Errors.Forbidden.ObservationSource`; Rust reports `invalid/context` for the same invalid source.
Both refuse before creating an observation or consuming admission.

The shared Rust kernel preserves the original principal, exact versioned plan and physical owner.
The observation context has an empty metadata frame; put correlation in the explicit payload.
Original cancelled contexts and expired frames remain unusable, and storage validates closed tokens.
Closing owners and version gates can refuse observation. Await it inside the still-owned physical
callback before returning; a logically cancelled caller may already have returned. The existing
runtime tracks physical work through cleanup. Releasing a completed temporary dependency does not
cancel observation. Cancellation remains cooperative and cannot undo or authorize replay of effects.

Calls made inside observation additionally require exact registered orb/object scopes and their
registered ancestors to exist and remain open. A missing scope returns `gone/scope`; a closing
scope or ancestor returns `conflict/scope_closed`. These checks apply to every dependent call;
ordinary calls retain their existing admission rules.
