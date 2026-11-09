# Architecture

Tembra has one lifecycle engine in Rust, called the **kernel**.
Language adapters keep application values and execute callbacks.
The engine decides when those callbacks can run.

## Rust crates

| Crate                                                        | Responsibility                                                                                                      |
| ------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------- |
| [tembra-core](../crates/core/src/lib.rs)                     | Own identities, references, call versions, state revisions and lifecycle order. It has no third-party dependencies. |
| [tembra](../crates/tembra/src/lib.rs)                        | Declare native Rust services. Keep typed values and run callbacks on an application executor.                       |
| [tembra-bridge](../crates/bridge/src/lib.rs)                 | Define typed kernel commands and results; generate platform bindings from the Rust command table.                   |
| [tembra-node-binding](../crates/node-binding/src/lib.rs)     | Expose the bridge through Node-API as `NodeKernel`.                                                                 |
| [tembra-browser-wasm](../crates/browser-wasm/src/lib.rs)     | Expose the bridge through WASM as `WasmKernel`.                                                                     |
| [tembra-mobile](../crates/mobile/src/lib.rs)                 | Expose the bridge to Swift and Kotlin as `NativeKernel` through UniFFI.                                             |
| [tembra-contracts](../crates/contracts/src/lib.rs)           | Execute prepared contracts; gate WIT authoring and real component verification behind the `authoring` feature.      |
| [tembra-contracts-wasm](../crates/contracts-wasm/src/lib.rs) | Decode prepared artifacts and validate bound method slots in browsers without a WIT parser.                         |
| [tembra-executor-wasm](../crates/executor-wasm/src/lib.rs)   | Load orb artifacts as ordinary `tembra::Orb` values and run them in Wasmtime.                                       |

## Lifecycle programs

A **scope** groups declarations owned by a host, orb or object.
A **program** orders activation, replacement or cleanup for those scopes.
An **effect** asks an adapter to open a resource, validate state or run another callback.

The adapter reports when each effect finishes.
The engine then schedules the next step.
Waiting programs use indexes that identify the changes they need.
The adapter does not choose a separate lifecycle order.

An **action** tells an adapter about pending work or a change, such as adopted state.
An execution action contains the effects for one program step.

## Language packages

| Package                                                                     | Responsibility                                                                                         |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| [@syntropika/tembra-sdk](../packages/ts/sdk/src/index.ts)                   | Declare orbs, keep JS values and execute effects. Also expose optional infrastructure and build tools. |
| [@syntropika/tembra-engine-node](../packages/ts/engine-node/package.json)   | Build and package the native Node binding and the interpreter module.                                  |
| [@syntropika/tembra-engine-wasm](../packages/ts/engine-wasm/package.json)   | Build and package the WASM kernel, contract module and browser interpreter module.                     |
| [TembraNative](../packages/swift/Package.swift)                             | Package Swift bindings and the Rust libraries for iOS and macOS.                                       |
| [tembra-kotlin](../packages/kotlin/build.gradle.kts)                        | Package the same Kotlin bindings for the JVM and Android.                                              |
| [@syntropika/tembra-react-native](../packages/ts/react-native/src/index.ts) | Connect the SDK to the mobile kernel through a TurboModule.                                            |

Node, browsers and React Native run trusted orb callbacks in their existing JavaScript runtime.
The WASM kernel module contains the kernel and bridge. It contains no JS interpreter, DOM or filesystem.

## Isolated orbs

An orb artifact is one immutable `.wasm` file. It carries a prepared WIT contract, an orb name and API
version, and either a TypeScript bundle or a compiled component. Build tooling produces it;
installation chooses its providers and grants. The artifact carries no authority.

| Host    | Source orbs                             | Compiled components |
| ------- | --------------------------------------- | ------------------- |
| Rust    | Wasmtime and dquickts                   | Wasmtime            |
| Node    | Wasmtime and dquickts                   | Wasmtime            |
| Browser | dquickts core module, host effect waits | Not yet supported   |

The interpreter is loaded lazily when the first source orb is installed. Each installed orb has
its own Wasm instance and heap. Isolated and trusted orbs share one host and one kernel lifecycle.
Trusted orbs are not sandboxed.

See [Wasm orb execution](../crates/executor-wasm/README.md).

The React Native adapter uses the shared Swift sources and Kotlin library.
Neither native package depends on React Native.

Each native kernel uses one worker thread. The kernel stays on that thread until shutdown.
Swift and Kotlin can send metadata requests from different threads.

Core features are optional: `hooks`, `subscriptions`, `views` and `telemetry`.
The `tembra` crate enables none by default. The mobile crate enables all four.
Check `host.features` in the SDK.

See [lifecycle programs](../crates/core/src/program/mod.rs) and [platform entrypoints](typescript.md).
