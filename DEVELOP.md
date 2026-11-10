# Development

This guide covers the development environment and repository checks.
For tag builds and registry publication, see the [release guide](docs/releases.md).
For runtime concepts and language APIs, see the [Tembra guide](docs/README.md).
For module layout and code style, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Tools

Use [mise](https://mise.jdx.dev/) to install and select the pinned development tools:

| Tool                           | Configuration                                                           | Purpose                                    |
| ------------------------------ | ----------------------------------------------------------------------- | ------------------------------------------ |
| Rust                           | [rust-toolchain.toml](rust-toolchain.toml)                              | Cargo, rustfmt, Clippy and the WASM target |
| Bun                            | [mise.toml](mise.toml) and [.bun-version](.bun-version)                 | TypeScript dependencies, builds and tests  |
| Node.js                        | [mise.toml](mise.toml) and [.node-version](.node-version)               | SDK compatibility tests for Node           |
| Just                           | [mise.toml](mise.toml) and [CI workflow](.github/workflows/quality.yml) | Repository commands                        |
| wasm-bindgen and cargo-machete | [justfile](justfile)                                                    | WASM bindings and dependency checks        |
| Chrome or Chromium             | `TEMBRA_BROWSER`                                                        | Browser and module Worker acceptance       |

Rustup reads the pinned Rust toolchain and installs its configured components and target.
From the repository root, install the mise tools, build tools and TypeScript dependencies:

```sh
mise trust
mise install
mise exec -- just install-tools
mise exec -- just install
mise exec -- just check-all
```

`mise exec` selects the repository's versions for the command and all child processes.
With [mise shell activation](https://mise.jdx.dev/getting-started.html#activate-mise)
enabled, the `bun`, `node` and `just` commands below use those versions automatically.
Keep the mise versions aligned with `.bun-version`, `.node-version`, `package.json`
and the CI workflow when updating tools. Older Bun versions can fail in the formatter's
Markdown worker path even when Node can format the same files.

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

Bun runs the repository build scripts. The release matrix also runs the Node-only engine
build script directly with the pinned Node version. Node runs SDK compatibility tests.

Engine package builds default to release mode. Pass `--debug` for a development build.
`just build-node` selects debug mode; `just build-wasm` selects release mode. Cargo compiles
Rust. The engine scripts package its output, and wasm-bindgen generates the browser loader.

The SDK build emits JavaScript and declarations. Bun can import SDK TypeScript sources directly.
Engine artifacts live in their owning package's `dist` directory.

## Isolated orbs

Source orbs run in a reusable dquickts interpreter core module. Build it once; orb changes do not
require rebuilding it. Compiled-only hosts do not need it.

The interpreter build requires WASI SDK 34 and the Rust nightly named by the justfile `nightly`
variable, with the `wasm32-wasip1` target. The compiled Rust provider fixture uses the same nightly
with the `wasm32-wasip3` target. The no-std probe uses `wasm32-unknown-unknown` with the component
linker and explicit WASI 0.3 bindings, so compiler runtime CLI imports cannot affect its tests.
The build pins and verifies the dquickts revision and the module's
exact private imports and exports. Without a dquickts checkout, it clones one into the cache.

```sh
rustup toolchain install "$(just --evaluate nightly)" --profile minimal --target wasm32-wasip1 --target wasm32-wasip3 --target wasm32-unknown-unknown
just build-interpreter <wasi-sdk> <cache-directory> [<dquickts-checkout>]
just build-provider <provider-target-directory>
just build-probe <provider-target-directory>
just build-consumer <provider-target-directory>
just build-contributor <provider-target-directory>
just build-http-client <provider-target-directory>
```

The interpreter recipe packages the same `interpreter.wasm` into `packages/ts/engine-node/dist` and
`packages/ts/engine-wasm/dist/interpreter`. Use reusable cache and target directories.

Runtime acceptance uses real artifacts. Build the engines, interpreter and SDK first. Then run
every isolated orb suite with an output directory outside the checkout:

```sh
just test-wasm-acceptance --out <directory> \
  --provider <provider-target-directory>/wasm32-wasip3/release/tembra_wasm_arithmetic.wasm \
  --probe <provider-target-directory>/wasm32-unknown-unknown/release/tembra_wasm_probe.wasm \
  --consumer <provider-target-directory>/wasm32-unknown-unknown/release/tembra_wasm_pipeline.wasm \
  --contributor <provider-target-directory>/wasm32-unknown-unknown/release/tembra_wasm_contributor.wasm \
  --http-client <provider-target-directory>/wasm32-unknown-unknown/release/tembra_wasm_http_client.wasm
```

The runner builds the fixture orbs into `<directory>/fixtures`, or reuses `--fixtures <directory>`.
It runs the Rust executor tests including artifact-dependent cases, the SDK Bun and Node suites, Chrome with the default and
override engines, `bun run test:browser`, the [Wasm orb example](examples/wasm-orb/README.md), and
packed consumers in Node, Bun and Chrome, including a clean NodeNext typecheck. The probe verifies
component quarantine, physical WASI settlement, filesystem grants and calls with a one-thread libuv pool.
Repeat `--suite native|sdk|node|browser|example|package` to run a subset. CI runs the same command in the
`acceptance` job of the quality workflow. `just test-wasm-orb` runs the Rust tests against supplied artifacts.

## Native builds

The Swift and Kotlin packages use UniFFI to generate bindings from the same Rust library.
Their TypeScript build scripts run with Bun. Neither package depends on React Native.

```sh
just build-mobile-bindings
```

For Swift, use macOS with Xcode installed:

```sh
just build-swift
just test-swift
```

The build creates `packages/swift/native/TembraFFI.xcframework` for iOS and macOS.
It contains arm64 device libraries and arm64/x86_64 libraries for the simulator and macOS.
The minimum versions are iOS 15.1 and macOS 11.
Swift Package Manager and CocoaPods compile the shared Swift sources.

The mobile FFI crate is an internal build target, like the Node binding; the Swift, Kotlin and React Native packages distribute its compiled libraries. React Native native packaging also copies the qualified `packages/ts/engine-node/dist/interpreter.wasm`; build that shared interpreter before packaging the mobile module.

The executor currently pins Wasmtime and `wasmtime-wasi` together to `51.0.0-rc.1`.
This prerelease includes the [Android `openat2` API-level guard](https://github.com/bytecodealliance/wasmtime/commit/b67df87c061d548508aa7b9be692392f14b80770),
which prevents the app sandbox from terminating compiled WASI filesystem calls on older Android API levels.
Qualify upgrades with the native, browser and mobile acceptance suites; an Android shell process does not exercise the app's seccomp restrictions.

For Kotlin/JVM, install Java 17 and Gradle 8.11.1:

```sh
just build-kotlin
just test-kotlin
```

This build does not require the Android SDK.
The JVM JAR includes the Rust library for the current host as a JNA resource.
Use `just build-kotlin --targets x86_64-unknown-linux-gnu,aarch64-unknown-linux-gnu` to select other targets.
Each target needs its Rust target and a compatible linker.
The build supports target libraries for Linux, macOS and Windows.

For Android, also install the Android SDK and NDK.
Set `ANDROID_HOME` and `ANDROID_NDK_HOME` to their installation directories.

```sh
cargo install cargo-ndk --locked
just build-kotlin-android
```

The default ABIs are `arm64-v8a` and `x86_64`. The minimum Android API is 24.
Use `just build-kotlin-android --abis arm64-v8a,armeabi-v7a,x86,x86_64` to build all supported ABIs.
Both builds write Maven packages and dependency metadata to `packages/kotlin/build/repository`.
The JVM artifact is `tembra-kotlin-jvm`. The Android artifact is `tembra-kotlin`.

## React Native builds

The adapter builds and packages the same native code:

```sh
bun run --cwd packages/ts/react-native build:ios
bun run --cwd packages/ts/react-native build:android
```

The iOS build copies the Swift package sources and XCFramework into the adapter archive.
CocoaPods compiles those sources with the React Native adapter.
The Android build copies the Kotlin Maven package. Gradle resolves it as a library dependency.
Build both platforms before you create a package archive. Git excludes generated sources and binaries.
Applications that install the archive do not need Rust or UniFFI.
See the [React Native guide](docs/react-native.md) for application setup.

## Examples

Run `just examples` for the native Rust examples.
See the [Rust guide](docs/rust.md) for their source and individual commands.

## Checks

```sh
just check
bun run test:browser
```

`just check` builds the engines and SDK. It checks formatting, TypeScript types and dependencies.
It runs Clippy, oxlint, Rust tests, SDK tests and React Native adapter tests.
It also runs Node compatibility tests, the WASM target check and the Rust examples.
It does not build the interpreter, so isolated orb tests skip. Browser and isolated orb acceptance
run separately; see [Isolated orbs](#isolated-orbs).

| Command                    | Purpose                                            |
| -------------------------- | -------------------------------------------------- |
| `just fmt`                 | Format Rust, TypeScript and project documents      |
| `just fmt-check`           | Check formatting without changing files            |
| `just lint`                | Run Clippy and oxlint                              |
| `just deps`                | Check for unused Rust dependencies                 |
| `just test`                | Build packages and run Rust, SDK and Node tests    |
| `just test-doc`            | Run Rustdoc examples with all features             |
| `just test-hook-lifecycle` | Build adapters and test the native hook lifecycle  |
| `just test-no-hooks`       | Test the core and bridge without optional features |
| `just check-wasm`          | Check the WASM target with all features            |
| `bun run typecheck`        | Check TypeScript across the workspace              |
| `bun run test:browser`     | Exercise Chrome and a module Worker                |

`just test` runs Rust tests and doctests across the workspace with all features enabled.
It also runs `just test-no-hooks` in a separate Cargo invocation, so adapter features cannot
enable hooks through feature unification. This recurring check exercises the core and bridge
with optional features disabled, including the bridge's explicit refusal of hook observation.
It is included in `just check`, `just check-all` and CI through `just test`.

Use `just test-doc` when changing public Rust guarantees or executable examples. It runs the
all-feature doctests alone; the full check already executes them through `just test`.
Use `just test-hook-lifecycle` for focused hook lifecycle checks.
It builds the Node and WASM engines and SDK before testing the core, native facade and Wasm executor.
Use `just test-no-hooks` directly when changing feature gates or shared bridge operations.

The browser runner uses `google-chrome` by default. Select another compatible executable with
`TEMBRA_BROWSER`, for example:

```sh
TEMBRA_BROWSER=chromium bun run test:browser
```

Native acceptance uses the actual Node addon. Isolated orb acceptance uses built artifacts; see
[Isolated orbs](#isolated-orbs).

React Native adapter tests use the Rust Node binding as a test transport.
For mobile acceptance, build an application and test it on each target platform.

Compare the same workloads when changing hot paths. Record the build profile, enabled features,
runtime and workload with every size or performance result. Keep profiling reports outside the
source tree.

To test the mobile execution transport with real interpreter and compiled-provider bytes on the current Rust host:

```sh
just test-mobile-wasm /path/to/interpreter.wasm /path/to/arithmetic-component.wasm /path/to/filesystem-probe.wasm
```

This checks portable execution, heap isolation, filesystem/network grants, compiled calls, foreign continuation delivery and invalidation. It complements application tests on iOS and Android; a successful host run does not establish handset qualification.
