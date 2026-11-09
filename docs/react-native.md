# React Native

Use `@syntropika/tembra-react-native` to run isolated TypeScript and compiled-language Wasm Orbs on iOS and Android.
The package requires React Native 0.82 or newer with the New Architecture.
Swift and Kotlin bindings connect its TurboModule to the Rust kernel.
Trusted application callbacks stay in Hermes. Isolated Orb code executes in its own Wasm memory through the native executor.
The mobile executor uses Wasmtime Pulley and does not generate executable native code.

## Install

Install `@syntropika/tembra-react-native` and the same version of `@syntropika/tembra-sdk` in your application.
Run CocoaPods installation for iOS. Rebuild the native application after installation.
React Native Codegen generates the native interfaces during that build.
Expo applications need a development build with native modules.
Use a JavaScript runtime on the device, such as Hermes.

For a source checkout, first build the native libraries in [DEVELOP.md](../DEVELOP.md#native-builds).

## Run an orb

```ts
import { createHost, orb } from "@syntropika/tembra-react-native";

const host = createHost();
try {
  await host.install(
    orb({
      name: "greeting",
      apiVersion: 1,
      services: {
        greeting: {
          methods: {
            hello() {
              return "Hello";
            },
          },
        },
      },
    }),
  );
  const reference = await host.get("greeting", "main");
  console.log(await reference.hello()); // Hello
  reference.release();
} finally {
  await host.close();
}
```

Use static imports for trusted application definitions. Downloaded or user-authored code must be installed as a prepared `.wasm` artifact, not evaluated in Hermes.

## Isolated Orbs and host APIs

Build source Orbs with the ordinary TypeScript tooling and WIT contract. The same artifacts run on the other supported hosts. Rust and other compiled-language components use the same installation facade and WIT value profile.

```ts
import { createHost, temporaryFilesystem } from "@syntropika/tembra-react-native";

const host = createHost({
  isolated: {
    providers: { filesystem: { scratch: temporaryFilesystem() } },
    onLog: ({ orb, level, message }) => console.log(orb, level, message),
  },
});

try {
  const response = await fetch(orbURL);
  if (!response.ok) throw new Error(`Orb request failed: ${response.status}`);
  await host.install({
    artifact: new Uint8Array(await response.arrayBuffer()),
    grants: {
      filesystem: [{ provider: "scratch", mount: "/data", writable: true }],
    },
  });
  // Acquire the service declared by the Orb's WIT contract.
} finally {
  await host.close();
}
```

`applicationFilesystem()` registers persistent app-private storage. `temporaryFilesystem()` registers disposable app storage. Both return directory adapters; a host can also supply its own directory path. Registration grants no Orb access. Each installation must grant a mount, and its WIT world must declare the corresponding WASI imports. No Node, OPFS or JCO details enter the application API.

The source profile exposes bounded UTF-8 reads/writes through `tembra.filesystem` and incremental byte streams through `tembra.transport`, as on the native desktop executor. Network providers require explicit destination grants and literal IP addresses. Streams close with their callback or its cancellation. The guest library `@syntropika/tembra-sdk/http` handles HTTP/1 and SSE over that transport, including backpressure and early reader closure. The callback and read deadlines still apply. DNS, TLS and a complete standard Fetch implementation are not provided. Compiled components use the executor's supported standard WASI imports and directory preopens. These capabilities remain narrower than complete WASI support; cryptographic guest randomness is not newly implemented by this mobile integration.

Guest factories stay synchronous. The native worker pauses at a host request while the mobile bridge asynchronously delivers its response. This avoids invoking Hermes callbacks synchronously from a native interpreter stack. The shared SDK still owns admission, state, references, resources, cancellation and reload. Native invalidation revokes pending replies and drains physical work outside the UI thread. Logging is attributed by the host and retained in bounded batches between native exchanges.

The native archive includes the same `interpreter.wasm` as the other native hosts. An optional `isolated.engine` byte array overrides it for qualification. Metadata-only hosts do not initialize the interpreter. `host.load()` still requires an application-supplied loader; `host.install({ artifact, grants })` accepts bytes directly.

## Close the host

Release references when you no longer need them.
Await `host.close()` to finish tasks and resource cleanup before the native kernel stops.
React Native module invalidation stops any remaining workers.
It cannot run application cleanup after the JavaScript runtime stops.

See [TypeScript](typescript.md) for the shared API and [Architecture](architecture.md) for the adapter boundaries.
