# React Native

Use `@syntropika/tembra-react-native` to run isolated TypeScript and compiled-language Wasm Orbs on iOS and Android.
The package requires React Native 0.88.1 or newer with the New Architecture and React 19.3 or newer.
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

Source and compiled Orbs share the native executor's standard WASI filesystem descriptors, granted directory preopens, clocks, randomness and CLI configuration. Filesystem bindings provide binary reads, streamed writes, metadata and directory operations. Resources may survive calls; pending operations and unfinished guest-written streams remain bound to their callback's lifetime.

`@syntropika/tembra-sdk/http` uses standard `wasi:http` resources for outgoing HTTP and incremental SSE. The native host handles HTTP framing, DNS and TLS; exact HTTP origin grants remain independent of raw socket grants. Request and response bodies stream with backpressure and cancellation. This helper is a limited Fetch facade, not a complete WHATWG Fetch implementation. See [TypeScript](typescript.md) for transport and collection limits.

Source and compiled Orbs use standard `wasi:sockets` TCP/UDP resources and name lookup under separate destination and resolution grants. Socket resources and their receive, send and listen streams have independent lifetimes; drop each resource when finished. Shared Rust/Pulley tests and generated bindings cover the common implementation, but do not establish physical iOS or Android device support for every operation. See the [WASI coverage matrix](../crates/executor-wasm/README.md#wasi-coverage).

Incoming HTTP uses `host.handleHttp` with the raw `IncomingHttpRequest` form described in
[TypeScript](typescript.md#incoming-http). The application owns listening and routing. Raw
header values, body chunks, trailers and typed refusals cross the TurboModule as binary
`ArrayBuffer` values, with demand-driven delivery. The raw facade uses
`AsyncIterable<Uint8Array>` and needs no WHATWG streams. The `Request`/`Response` convenience is
unsupported on the qualified React Native 0.88.1 runtime: Hermes has no `ReadableStream`, and
its fetch `Request` exposes no body stream. Tembra refuses that form before dispatching the
handler. Use the raw form to preserve request bytes.

Guest factories stay synchronous. The native worker pauses at a host request while the mobile bridge asynchronously delivers its response. This avoids invoking Hermes callbacks synchronously from a native interpreter stack. The shared SDK still owns admission, state, references, resources, cancellation and reload. Native invalidation revokes pending replies and drains physical work outside the UI thread. Logging is attributed by the host and retained in bounded batches between native exchanges.

Every source Orb is a prepared component carrying its JavaScript runtime. The native archive needs no separate interpreter asset or engine option. Metadata-only hosts do not instantiate components. `host.load()` still requires an application-supplied loader; `host.install({ artifact, grants })` accepts bytes directly.

## Close the host

Release references when you no longer need them.
Await `host.close()` to finish tasks and resource cleanup before the native kernel stops.
React Native module invalidation stops any remaining workers.
It cannot run application cleanup after the JavaScript runtime stops.

See [TypeScript](typescript.md) for the shared API and [Architecture](architecture.md) for the adapter boundaries.
