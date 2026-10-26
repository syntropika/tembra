# Architecture

Tembra has one lifecycle engine in Rust, called the **kernel**.
Language adapters keep application values and execute callbacks.
The engine decides when those callbacks can run.

## Rust crates

| Crate                                                      | Responsibility                                                                                                      |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| [tembra-core](../crates/core/src/lib.rs)                   | Own identities, references, call versions, state revisions and lifecycle order. It has no third-party dependencies. |
| [tembra](../crates/tembra/src/lib.rs)                      | Declare native Rust services. Keep typed values and run callbacks on an application executor.                       |
| [tembra-bridge](../crates/bridge/src/lib.rs)               | Convert metadata commands and results to JSON envelopes.                                                            |
| [tembra-node-binding](../crates/node-binding/src/lib.rs)   | Expose the bridge through Node-API as `NodeKernel`.                                                                 |
| [tembra-browser-wasm](../crates/browser-wasm/src/lib.rs)   | Expose the bridge through WASM as `WasmKernel`.                                                                     |
| [tembra-mobile](../crates/mobile/src/lib.rs)               | Expose the bridge to Swift and Kotlin as `NativeKernel` through UniFFI.                                             |
| [tembra-executor-js](../crates/executor-js/src/runtime.rs) | Run bundled JavaScript in V8 beside native Rust services. Both use the same kernel.                                 |

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

## TypeScript packages

| Package                                                          | Responsibility                                                                                         |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| [@tembra/sdk](../packages/ts/sdk/src/index.ts)                   | Declare orbs, keep JS values and execute effects. Also expose optional infrastructure and build tools. |
| [@tembra/engine-node](../packages/ts/engine-node/package.json)   | Build and package the native Node binding.                                                             |
| [@tembra/engine-wasm](../packages/ts/engine-wasm/package.json)   | Build and package the WASM module, loader and types.                                                   |
| [@tembra/react-native](../packages/ts/react-native/src/index.ts) | Connect the SDK to the mobile kernel through a TurboModule.                                            |

Node, browsers and React Native run callbacks in their existing JavaScript runtime.
WASM contains the kernel and bridge. It contains no JS interpreter, DOM or filesystem.
The embedded Rust integration uses one V8 runtime per host.

Each mobile host uses one worker thread. The kernel stays on that thread until shutdown.
Swift and Kotlin can send metadata requests from different threads.

Core features are optional: `hooks`, `subscriptions`, `views` and `telemetry`.
The `tembra` crate enables none by default. The mobile crate enables all four.
Check `host.features` in the SDK.

See [lifecycle programs](../crates/core/src/program/mod.rs) and [platform entrypoints](typescript.md).
