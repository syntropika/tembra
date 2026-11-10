# Tembra

Tembra is a runtime for applications built from composable modules called **orbs**. It connects their
services, manages shared state and resources, and coordinates updates while work is running.

Orbs own their behavior and validation. Tembra manages how they work together throughout their
lifetime through a shared Rust engine. Applications can combine Rust and TypeScript across
servers, browsers, Web Workers and React Native.

An orb is either trusted application code (Rust, or TypeScript in the host runtime) or a built
`.wasm` artifact that runs isolated in WebAssembly. Both kinds install into the same host and use
the same lifecycle.

## What it provides

- **Composable orbs.** Declare the services an orb provides and the dependencies it consumes.
- **Isolated orbs.** Build TypeScript or a compiled component against a WIT contract, then grant
  filesystem and network access per orb at installation.
- **Consistent state.** Validate changes before publishing them and observe the values that matter.
- **Controlled updates.** Replace implementations and migrate state while respecting active work.
- **Shared resources.** Tie resources and tasks to the host, orb or object that owns them.
- **Operation context.** Carry typed metadata through calls without changing their business arguments.

## Repository

| Directory                                                                    | Contents                                                                                       |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| [crates/core/](crates/core/)                                                 | Shared Rust engine for ownership, state revisions and orb updates                              |
| [crates/tembra/](crates/tembra/)                                             | Single Rust facade for native and isolated Wasm orbs                                           |
| [crates/bridge/](crates/bridge/)                                             | Communication between the engine and its platform integrations                                 |
| [crates/contracts/](crates/contracts/)                                       | WIT contracts, orb artifacts and installation grants                                           |
| [crates/executor-wasm/](crates/executor-wasm/)                               | Runs orb artifacts in Wasmtime for Rust hosts                                                  |
| [crates/node-binding/](crates/node-binding/)                                 | Connects Node.js to the Rust engine                                                            |
| [crates/mobile/](crates/mobile/)                                             | Shared Rust kernel bindings for Swift and Kotlin                                               |
| [crates/browser-wasm/](crates/browser-wasm/)                                 | Makes the Rust engine available in browsers and Workers                                        |
| [crates/orb-macro/](crates/orb-macro/)                                       | Derives a compiled orb's WIT contract and bindings from a Rust module                          |
| [crates/cargo-tembra/](crates/cargo-tembra/)                                 | `cargo tembra`: builds, inspects and packages Rust orbs                                        |
| [packages/ts/sdk/](packages/ts/sdk/)                                         | TypeScript SDK and orb tooling                                                                 |
| [packages/ts/engine-node/](packages/ts/engine-node/)                         | Native engine packaging and builds                                                             |
| [packages/ts/engine-wasm/](packages/ts/engine-wasm/)                         | WASM engine packaging and builds                                                               |
| [packages/swift/](packages/swift/)                                           | Swift bindings for iOS and macOS                                                               |
| [packages/kotlin/](packages/kotlin/)                                         | Kotlin bindings for the JVM and Android                                                        |
| [packages/ts/react-native/](packages/ts/react-native/)                       | React Native TurboModule, native packaging and mobile builds                                   |
| [packages/ts/test/](packages/ts/test/)                                       | Browser integration tests                                                                      |
| [examples/rust/](examples/rust/)                                             | Native orbs, identity, execution and resource lifetime                                         |
| [examples/wasm-orb/](examples/wasm-orb/)                                     | A TypeScript orb and a compiled Rust orb built from one WIT file                               |
| [examples/rust-orb-service/](examples/rust-orb-service/)                     | A compiled Rust orb exporting one service, written from its code and built with `cargo tembra` |
| [examples/rust-orb-hook-contribution/](examples/rust-orb-hook-contribution/) | A compiled Rust orb contributing to a native orb's hook, counting in a root resource           |
| [examples/rust-orb-state/](examples/rust-orb-state/)                         | A compiled Rust orb whose state the kernel keeps, observed and reloaded by the host            |

## Development

The [Wasm orb example](examples/wasm-orb/README.md) builds a TypeScript orb with an npm dependency
and a compiled Rust provider, then installs both artifacts in a Node host. The
[Rust orb example](examples/rust-orb-service/README.md) writes a compiled orb from a Rust module with
`#[orb]` and builds its artifact with `cargo tembra`.

See the [guide](docs/README.md) for concepts and Rust and TypeScript usage.
See [DEVELOP.md](DEVELOP.md) for setup, tools, builds, tests and technical details.
See [CONTRIBUTING.md](CONTRIBUTING.md) for code structure and documentation conventions.

## Acknowledgements

Tembra draws on [Chord](https://github.com/earendil-works/pi/tree/e17e2e10556cea6b674bb627726bef8ceed90065/packages/chord)
for its managed object model, reactive state and reload semantics. Thanks to its authors and
contributors.
