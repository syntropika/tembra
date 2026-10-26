# React Native

Use `@tembra/react-native` to run TypeScript orbs on iOS and Android.
The package requires React Native 0.82 or newer with the New Architecture.
Swift and Kotlin bindings connect its TurboModule to the Rust kernel.
Application values and callbacks stay in the JavaScript runtime.

## Install

Install `@tembra/react-native` and the same version of `@tembra/sdk` in your application.
Run CocoaPods installation for iOS. Rebuild the native application after installation.
React Native Codegen generates the native interfaces during that build.
Expo applications need a development build with native modules.
Use a JavaScript runtime on the device, such as Hermes.

For a source checkout, first build the native libraries in [DEVELOP.md](../DEVELOP.md#mobile-builds).

## Run an orb

```ts
import { createHost, orb } from "@tembra/react-native";

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

Use static imports so Metro can bundle the orb definitions.
`host.load()` requires an application-supplied `load` callback.
The package does not include a JavaScript interpreter or a loader for downloaded source.
Swift and Kotlin expose the metadata kernel. Declare application orbs in TypeScript.

## Close the host

Release references when you no longer need them.
Await `host.close()` to finish tasks and resource cleanup before the native kernel stops.
React Native module invalidation stops any remaining workers.
It cannot run application cleanup after the JavaScript runtime stops.

See [TypeScript](typescript.md) for the shared API and [Architecture](architecture.md) for the adapter boundaries.
