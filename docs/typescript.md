# TypeScript

The SDK declares JS orbs and runs their callbacks.
Rust checks ownership and lifecycle transitions.
Application values stay in the JS runtime until a language or transport boundary needs a codec.

## Run an orb in Node

```ts
import { orb, type Context, type State } from "@syntropika/tembra-sdk";
import { createHost } from "@syntropika/tembra-sdk/node";

type Counter = { count: State<number> };

const counter = orb({
  name: "counter",
  apiVersion: 1,
  services: {
    counter: {
      create(self) {
        return { count: self.state(0, { validate: Number.isSafeInteger }) };
      },
      methods: {
        increment({ count }: Counter, ctx: Context) {
          return count.update(ctx, (value) => value + 1);
        },
      },
    },
  },
});

const host = createHost();
try {
  await host.install(counter);
  const reference = await host.get("counter", "main");
  console.log(await reference.increment()); // 1
  reference.release();
} finally {
  await host.close();
}
```

Factories finish synchronously. Methods and resource callbacks can be asynchronous.
`host.get()` waits until the object is ready.
A method call without an explicit context starts its own root.
Pass an existing context as the first argument to join that operation.
Returning a context or copying an opaque token does not keep its issuing call alive.
See [context lifetime](context.md) before sharing a binding beyond a method return.

`get()` returns a dynamic reference. A service name does not infer its argument and result types.
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

The shared Rust kernel preserves the original principal, exact versioned plan and physical owner.
The observation context has an empty metadata frame; put correlation in the explicit payload.
Original cancelled contexts and expired frames remain unusable, and storage validates closed tokens.
Closing owners and version gates can refuse observation. Await it inside the still-owned physical
callback before returning; a logically cancelled caller may already have returned. The existing
runtime tracks physical work through cleanup. Releasing a completed temporary dependency does not
cancel observation. Cancellation remains cooperative and cannot undo or authorize replay of effects.
