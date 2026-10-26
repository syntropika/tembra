# Tembra for React Native

`@tembra/react-native` connects the Tembra TypeScript SDK to the shared Rust engine through
Swift on iOS and Kotlin on Android. It targets React Native 0.82 and newer with the New Architecture.
The integration is validated against React Native 0.82.1.

Application values, state and callbacks remain in the application's JavaScript realm. The synchronous
TurboModule exchanges kernel metadata only. UniFFI generates Swift and Kotlin bindings from the
same Rust interface. Each host has its own worker, so React Native calls and invalidation can arrive
on different threads without moving the thread-affine kernel.

## Application use

Install `@tembra/react-native` and the matching `@tembra/sdk` version, then rebuild the native app.
Run CocoaPods installation for iOS. Expo applications require a development build with native modules.

```ts
import { createHost, orb } from "@tembra/react-native";

const host = createHost({ name: "app" });
await host.install(
  orb({
    name: "counter",
    apiVersion: 1,
    services: {
      counter: {
        create(self) {
          return { state: self.state({ value: 0 }) };
        },
        methods: {
          increment({ state }, context) {
            state.update(context, (current: { value: number }) => ({ value: current.value + 1 }));
          },
        },
      },
    },
  }),
);
const counter = await host.get("counter", "main");
await counter.increment();
counter.release();
await host.close();
```

Use statically imported orb definitions bundled by Metro. `host.load()` is available only when you
supply a `load` callback in the host options. The package does not evaluate downloaded source or
embed another JavaScript engine. Swift and Kotlin expose the metadata kernel; native orb authoring
is outside this adapter's API.

Await `host.close()` when an application-owned host is no longer needed. This drains tasks and
resource cleanup before releasing native storage. Module invalidation releases any remaining workers
during React Native teardown, when JavaScript cleanup can no longer be guaranteed. Synchronous
metadata requests require an on-device JavaScript runtime, such as Hermes.

## Build native artifacts

Run commands from the repository root. Install Rust and Bun using the repository's toolchain files.

```sh
bun install --frozen-lockfile
bun run --cwd packages/ts/react-native build:bindings
```

The TypeScript build script uses Bun and the pinned UniFFI generator. Generated language sources and
binaries are ignored by Git. Generate the artifacts before packing or publishing this package.
Consumers of a prepared package do not need Rust or UniFFI.

On macOS with Xcode installed:

```sh
bun run --cwd packages/ts/react-native build:ios
```

This builds iOS arm64 and simulator arm64/x86_64 libraries, then creates
`native/TembraFFI.xcframework`. CocoaPods compiles the generated Swift and the thin Objective-C++
TurboModule adapter. The minimum iOS deployment target is 15.1.

For Android, install the Android NDK and `cargo-ndk`, and configure `ANDROID_NDK_HOME`:

```sh
cargo install cargo-ndk --locked
bun run --cwd packages/ts/react-native build:android
# Select additional Android ABIs when needed.
bun run --cwd packages/ts/react-native build:android --abis arm64-v8a,armeabi-v7a,x86,x86_64
```

The default ABIs are arm64-v8a and x86_64. The minimum Android API is 24. Gradle compiles the generated
Kotlin bindings and includes JNA and the shared libraries from `android/src/main/jniLibs`.
React Native Codegen generates the native TurboModule interfaces when the consuming app builds.

## Verification

```sh
cargo test --locked -p tembra-mobile --all-features
bun run --cwd packages/ts/react-native typecheck
just build-node
bun run --cwd packages/ts/react-native test
```

Rust tests cover metadata envelopes, host isolation, concurrent calls and release. TypeScript tests
exercise state, reload and cleanup through a test TurboModule transport backed by the real Rust
Node binding. Native app builds and device execution provide the final platform acceptance checks.
