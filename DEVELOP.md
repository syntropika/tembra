# Development

This guide covers the development environment and repository checks.
For runtime concepts and language APIs, see the [Tembra guide](docs/README.md).
For module layout and code style, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Tools

Use the pinned tool versions:

| Tool                           | Configuration                                          | Purpose                                    |
| ------------------------------ | ------------------------------------------------------ | ------------------------------------------ |
| Rust                           | [rust-toolchain.toml](rust-toolchain.toml)             | Cargo, rustfmt, Clippy and the WASM target |
| Bun                            | [.bun-version](.bun-version)   | TypeScript dependencies, builds and tests  |
| Node.js                        | [.node-version](.node-version) | SDK compatibility tests for Node   |
| Just                           | [CI workflow](.github/workflows/quality.yml)           | Repository commands                        |
| wasm-bindgen and cargo-machete | [justfile](justfile)                                   | WASM bindings and dependency checks        |
| Chrome or Chromium             | `TEMBRA_BROWSER`                                       | Browser and module Worker acceptance       |

Rustup reads the pinned Rust toolchain and installs its configured components and target.
Install Just, then install the build tools and TypeScript dependencies:

```sh
cargo install just --version 1.21.0 --locked
just install-tools
just install
```

Run these commands from the repository root.
The root manifest defines the Bun workspaces, development tools and lockfile.
TypeScript packages are in `packages/ts`.

## Build

Build the engines before the SDK so generated WASM declarations are available:

```sh
just build-node
just build-wasm
just build-sdk
```

Each package also owns its build command:

```sh
bun run --cwd packages/ts/engine-node build --features=hooks,subscriptions,views,telemetry
bun run --cwd packages/ts/engine-wasm build --features=hooks,subscriptions,views
bun run --cwd packages/ts/sdk build
```

Engine package builds default to release mode. Pass `--debug` for a development build.
`just build-node` selects debug mode; `just build-wasm` selects release mode. Cargo compiles
Rust. The engine scripts package its output, and wasm-bindgen generates the browser loader.

The SDK build emits JavaScript, declarations and the bundled embedded adapter. Bun can import
SDK TypeScript sources directly. Engine artifacts live in their owning package's `dist` directory.

## Examples

Run `just examples` for the native Rust examples.
See the [Rust guide](docs/rust.md) for their source and individual commands.

## Checks

```sh
just check
bun run test:browser
```

`just check` builds the engines and SDK, then runs formatting, strict TypeScript, lint, Clippy,
dependency checks, Rust tests, SDK tests, Node acceptance and the WASM target check.
Browser acceptance runs separately against the built WASM engine.

| Command                                  | Purpose                                         |
| ---------------------------------------- | ----------------------------------------------- |
| `just fmt`                               | Format Rust, TypeScript and project documents   |
| `just fmt-check`                         | Check formatting without changing files         |
| `just lint`                              | Run Clippy and oxlint                           |
| `just deps`                              | Check for unused Rust dependencies              |
| `just test`                              | Build packages and run Rust, SDK and Node tests |
| `just check-wasm`                        | Check the WASM target with all features         |
| `bun run typecheck`    | Check TypeScript across the workspace           |
| `bun run test:browser` | Exercise Chrome and a module Worker             |

The browser runner uses `google-chrome` by default. Select another compatible executable with
`TEMBRA_BROWSER`, for example:

```sh
TEMBRA_BROWSER=chromium bun run test:browser
```

Native acceptance uses the actual Node addon. Embedded acceptance uses V8 with a native Rust
provider through the same kernel. Build the SDK before running embedded tests directly.

Compare the same workloads when changing hot paths. Record the build profile, enabled features,
runtime and workload with every size or performance result. Keep profiling reports outside the
source tree.
