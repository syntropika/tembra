# Tembra guide

Tembra connects services from composable modules called **orbs**.
This guide explains the concepts and the code that implements them.

## Start here

1. Read [Architecture](architecture.md) for the crate and package map.
2. Read [Orbs and objects](orbs.md) for names, services and references.
3. Run the [Rust example](rust.md) or the [TypeScript example](typescript.md).
4. Build an isolated orb with the [Wasm orb example](../examples/wasm-orb/README.md).

## Concepts

| Guide                                     | Question                                              |
| ----------------------------------------- | ----------------------------------------------------- |
| [Calls and context](context.md)           | What can a call do, and which metadata does it carry? |
| [State](state.md)                         | How does a checked change become visible?             |
| [Resources and tasks](lifetime.md)        | What keeps work alive, and when does cleanup finish?  |
| [Updates](updates.md)                     | How does new code replace old code?                   |
| [Hooks and events](extensions.md)         | How do orbs extend behavior or receive events?        |
| [Data and remote calls](communication.md) | What crosses a language or host boundary?             |
| [Errors and diagnostics](diagnostics.md)  | How can an application inspect failures and work?     |

## Use the runtime

| Guide                           | Purpose                                           |
| ------------------------------- | ------------------------------------------------- |
| [Rust](rust.md)                 | Declare native services and supply an executor.   |
| [TypeScript](typescript.md)     | Use the SDK in Node, a browser or a Worker.       |
| [Swift](swift.md)               | Use native kernel bindings on iOS and macOS.      |
| [Kotlin](kotlin.md)             | Use native kernel bindings on the JVM or Android. |
| [React Native](react-native.md) | Use the SDK on iOS and Android.                   |
| [Build tools](tooling.md)       | Build isolated orbs into `.wasm` artifacts.       |

For setup and repository checks, see [DEVELOP.md](../DEVELOP.md).
For code style, see [CONTRIBUTING.md](../CONTRIBUTING.md).

For hook admission, ownership and composition guarantees, see [Hook decisions](decisions.md)
and [Runtime contract guarantees](contracts.md).
