# Tembra for React Native

`@syntropika/tembra-react-native` runs isolated TypeScript and compiled-language Wasm Orbs on iOS and Android.
Swift and Kotlin bindings connect the SDK to the shared Rust kernel, portable Wasm executor and app-private filesystem adapters.
The package requires React Native 0.88.1 or newer with the New Architecture and React 19.3 or newer.
Hermes V1 is the qualified runtime; the host must provide `WeakRef`, `FinalizationRegistry` and `Symbol.asyncIterator`.

See the [React Native guide](../../../docs/react-native.md) for installation, use and cleanup.
See [Native builds](../../../DEVELOP.md#native-builds) to generate bindings and package native libraries.
See [Architecture](../../../docs/architecture.md) for the crate and package boundaries.
