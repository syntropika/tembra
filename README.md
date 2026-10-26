# Tembra

Tembra is a runtime for applications built from composable modules called **orbs**. It connects their
services, manages shared state and resources, and coordinates updates while work is running.

Orbs own their behavior and validation. Tembra manages how they work together throughout their
lifetime through a shared Rust engine. Applications can combine Rust and TypeScript across
servers, browsers and Web Workers.

## What it provides

- **Composable orbs.** Declare the services an orb provides and the dependencies it consumes.
- **Consistent state.** Validate changes before publishing them and observe the values that matter.
- **Controlled updates.** Replace implementations and migrate state while respecting active work.
- **Shared resources.** Tie resources and tasks to the host, orb or object that owns them.
- **Operation context.** Carry typed metadata through calls without changing their business arguments.

## Repository

| Directory                                            | Contents                                                          |
| ---------------------------------------------------- | ----------------------------------------------------------------- |
| [crates/core/](crates/core/)                         | Shared Rust engine for ownership, state revisions and orb updates |
| [crates/tembra/](crates/tembra/)                     | Rust interface for declaring and running orbs                     |
| [crates/bridge/](crates/bridge/)                     | Communication between the engine and its platform integrations    |
| [crates/executor-js/](crates/executor-js/)           | Embedded JavaScript execution through V8 for Rust applications    |
| [crates/node-binding/](crates/node-binding/)         | Connects Node.js to the Rust engine                               |
| [crates/browser-wasm/](crates/browser-wasm/)         | Makes the Rust engine available in browsers and Workers           |
| [packages/ts/sdk/](packages/ts/sdk/)                 | TypeScript SDK and orb tooling                                    |
| [packages/ts/engine-node/](packages/ts/engine-node/) | Native engine packaging and builds                                |
| [packages/ts/engine-wasm/](packages/ts/engine-wasm/) | WASM engine packaging and builds                                  |
| [packages/ts/test/](packages/ts/test/)               | Browser integration tests                                         |
| [examples/rust/](examples/rust/)                     | Native orbs, identity, execution and resource lifetime            |

## Development

See the [guide](docs/README.md) for concepts and Rust and TypeScript usage.
See [DEVELOP.md](DEVELOP.md) for setup, tools, builds, tests and technical details.
See [CONTRIBUTING.md](CONTRIBUTING.md) for code structure and documentation conventions.

## Acknowledgements

Tembra draws on [Chord](https://github.com/earendil-works/pi/tree/e17e2e10556cea6b674bb627726bef8ceed90065/packages/chord)
for its managed object model, reactive state and reload semantics. Thanks to its authors and
contributors.
