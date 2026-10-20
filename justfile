# Project commands. Keep arguments positional so paths and shell characters stay data.
set positional-arguments

# List available commands.
default:
    @just --list

# Activate rust-toolchain.toml, install pinned development tools, and enable hooks.
setup: toolchain install-tools install-hooks

# Rustup reads rust-toolchain.toml and installs its toolchain, components, and target.
toolchain:
    rustup show active-toolchain

# Install the pinned native development tools outside the runtime dependency graph.
install-tools:
    cargo install --locked cargo-machete --version 0.9.2
    cargo install --locked git-cliff --version 2.14.0
    cargo install --locked wasm-bindgen-cli --version 0.2.129

# Apply the repository's Rust formatting rules.
fmt:
    cargo fmt --all
    rustfmt --edition 2024 testing/rust/tests/codegen/fixtures/consumer.rs testing/rust/tests/codegen/fixtures/native_consumer.rs

# Refuse Rust formatting changes without editing files.
fmt-check:
    @cargo fmt --all --check || { printf '%s\n' 'Rust formatting differs; run `just fmt`, then review the changes.' >&2; exit 1; }
    @rustfmt --edition 2024 --check testing/rust/tests/codegen/fixtures/consumer.rs testing/rust/tests/codegen/fixtures/native_consumer.rs || { printf '%s\n' 'Rust fixture formatting differs; run `just fmt`, then review the changes.' >&2; exit 1; }

# Check every native Rust target with warnings treated as errors.
lint:
    cargo clippy --locked --workspace --all-targets --features tembra-testing/embedded-executor -- -D warnings

# Refuse unused Rust dependencies.
deps:
    cargo machete

# Run Rust tests; optional arguments are forwarded to Cargo.
test *args:
    cargo test --locked --workspace --features tembra-testing/embedded-executor "$@"

# Run every non-unit suite managed by the root testing directory.
test-integration:
    cargo test --locked -p tembra-testing --features embedded-executor
    bun run --filter @tembra/testing test
    bun run --filter @tembra/testing test:browser

# Run the centralized Rust integrations, optionally filtering Cargo test targets.
test-integration-rust *args:
    cargo test --locked -p tembra-testing --features embedded-executor "$@"

# Run centralized TypeScript package, tooling and actual Node integrations.
test-integration-typescript:
    bun run --filter @tembra/testing test

# Run centralized browser integrations in real Chromium, including Web Workers.
test-integration-browser:
    bun run --filter @tembra/testing test:browser

# Run executable Rust documentation examples.
test-doc:
    cargo test --locked --workspace --doc

# Verify the public typed Rust facade and declarative contract expansion.
test-dx *args:
    cargo test --locked -p tembra --all-features "$@"
    cargo test --locked -p tembra-testing --features embedded-executor --test 'tembra-*' "$@"

# Verify external Rust binding generation and compiled consumers.
test-codegen *args:
    cargo test --locked -p tembra-codegen "$@"
    cargo test --locked -p tembra-testing --test 'codegen-*' "$@"

# Run the native typed Rust example using the platform-neutral driver.
run-rust-example:
    cargo run --locked -p tembra-example-rust

# Lint the optional native facade as well as its default shared API.
lint-dx:
    cargo clippy --locked -p tembra --all-features --all-targets -- -D warnings

# Execute native runner acceptance without repeating default facade tests.
test-native-facade:
    cargo test --locked -p tembra-testing --test tembra-api-integration native_runner

# Compile the public facade without native scheduling dependencies for browsers.
check-dx-wasm:
    cargo check --locked -p tembra --target wasm32-unknown-unknown

# Verify core changes independently while language adapters are being integrated.
test-core *args:
    cargo test --locked -p tembra-core "$@"
    cargo test --locked -p tembra-testing --test 'core-*' "$@"

test-core-doc:
    cargo test --locked -p tembra-core --doc

lint-core:
    cargo clippy --locked -p tembra-core --all-targets -- -D warnings

fmt-core:
    cargo fmt -p tembra-core

check-core-wasm:
    cargo check --locked -p tembra-core -p tembra-contracts --target wasm32-unknown-unknown

# Check the shared browser core, excluding native-only external tooling.
check-wasm:
    cargo check --locked -p tembra-core -p tembra-contracts -p tembra-browser-wasm -p tembra-hooks --all-features --target wasm32-unknown-unknown

# Verify the pure hook manager without either JavaScript engine adapter.
test-hooks *args:
    cargo test --locked -p tembra-hooks "$@"
    cargo test --locked -p tembra-testing --test 'hooks-*' "$@"

lint-hooks:
    cargo clippy --locked -p tembra-hooks --all-targets -- -D warnings

fmt-hooks:
    cargo fmt -p tembra-hooks

check-hooks-wasm:
    cargo check --locked -p tembra-hooks --target wasm32-unknown-unknown

# Check the native JavaScript adapter independently during core/browser integration.
check-executor:
    cargo check --locked -p tembra-executor-js

# Run native JavaScript executor tests; optional Cargo arguments remain positional data.
test-executor *args:
    cargo test --locked -p tembra-executor-js "$@"
    cargo test --locked -p tembra-testing --features embedded-executor --test 'executor-js-*' "$@"

# Check every native JavaScript target with the inherited strict lints.
lint-executor:
    cargo clippy --locked -p tembra-executor-js --all-targets -- -D warnings

# Format only the native JavaScript adapter while other modules are being edited.
fmt-executor:
    cargo fmt -p tembra-executor-js

# Resolve an intentional workspace dependency change while preserving external lock choices.
lock:
    cargo update --workspace

# Update the JavaScript lockfile after intentional package changes.
lock-ts:
    bun install --lockfile-only

# Typecheck only the browser facade during shared core integration.
check-browser-ts:
    bun run --bun --filter @tembra/sdk typecheck

# Check browser-only Rust code with the inherited strict lints.
lint-browser-wasm:
    cargo clippy --locked -p tembra-browser-wasm -p tembra-hooks --all-features --target wasm32-unknown-unknown -- -D warnings

# Format the browser adapter while other shared modules are being integrated.
fmt-browser-wasm:
    cargo fmt -p tembra-browser-wasm

# Build browser bindings with the CLI version matching the pinned Rust crate.
build-browser-wasm:
    cargo build --locked -p tembra-browser-wasm --target wasm32-unknown-unknown
    wasm-bindgen --target web --out-dir target/browser-wasm --out-name tembra_browser_wasm target/wasm32-unknown-unknown/debug/tembra_browser_wasm.wasm

# Run the reusable native WebSocket acceptance server on a configurable address.
run-rpc-server address='127.0.0.1:0':
    cargo run --locked -p tembra-testing --bin rpc-server -- "$1"

# Emit the native-authored browser acceptance contract without opening a listener.
emit-rpc-contract:
    cargo run --locked --quiet -p tembra-testing --bin rpc-server -- --descriptor

# Run all Rust gates.
check: fmt-check lint deps test check-wasm

# Typecheck and execute the TypeScript SDK, integrated tooling and example checks.
check-ts: build-browser-wasm build-tooling-generators build-ts build-node-binding
    bun run typecheck
    bun run test

# Apply the shared TypeScript/JavaScript formatting policy.
fmt-ts:
    bun run format

# Format only the specified files during parallel integration.
fmt-ts-files *args:
    bun --bun x --no-install oxfmt "$@"

# Lint only the specified files using the complete shared policy.
lint-ts-files *args:
    bun --bun x --no-install oxlint --disable-nested-config --deny-warnings --report-unused-disable-directives "$@"

# Run selected TypeScript source tests with Bun without rebuilding other packages.
test-ts-files *args:
    bun test "$@" --timeout 30000

# Compile one package while preserving dependency build ordering.
build-ts-package package:
    bun run --bun --filter "$1" build

# Typecheck one package without rebuilding unrelated consumers.
typecheck-ts-package package:
    bun run --bun --filter "$1" typecheck

# Refuse formatting drift and type-aware lint warnings.
lint-ts: build-ts
    bun run format:check
    bun run lint

# Run the complete native, browser-core, and TypeScript checks.
check-all: check lint-browser-wasm check-ts lint-ts lint-dx check-dx-wasm test-native-facade run-rust-example

# Compile all native Rust packages.
build:
    cargo build --locked --workspace

# Compile the SDK and integrated development tools.
build-ts:
    bun run --bun --filter @tembra/sdk build

# Build Rust exporters and the external Rust generator used by tooling tests.
build-tooling-generators:
    cargo build --locked -p tembra-testing --bin tembra-testing-emit-contract
    cargo build --locked -p tembra-codegen

check-tooling *args: build-tooling-generators
    bun run --bun --filter @tembra/sdk typecheck
    bun run --bun --filter @tembra/testing test:tooling "$@"

# Install exactly the committed JavaScript dependency graph.
install-ts:
    bun install --frozen-lockfile

# Run a typed application example, defaulting to native local values.
run-example name='local':
    cargo run --locked -p tembra-example-rust --example "$1"

# Generate Rust DTOs and typed contract wrappers; stdout is the default output.
codegen-rust input='-' output='-' bindings='':
    if [ -n "$3" ]; then cargo run --locked --quiet -p tembra-codegen -- "$1" "$2" "$3"; else cargo run --locked --quiet -p tembra-codegen -- "$1" "$2"; fi

# Emit the independently authored Serde/Schemars contract to stdout.
emit-contract-rust:
    cargo run --locked --quiet -p tembra-testing --bin tembra-testing-emit-contract

# Emit the independently authored TypeBox contract to stdout.
emit-contract-ts:
    bun run --bun --filter @tembra/sdk build >&2
    bun run testing/typescript/scripts/emit-counter.ts

# Enable the versioned hooks in this checkout; no source files are staged or edited.
install-hooks:
    chmod +x .githooks/pre-commit
    git config --local core.hooksPath .githooks

# Render a changelog to stdout; writing a file requires explicit CLI arguments.
changelog *args:
    git-cliff --config cliff.toml "$@"

# Run an authored TypeScript tool with the pinned Bun runtime.
run-ts *args:
    bun run "$@"

# Build and stage the SDK's Node-API addon without publishing a second package.
build-node-binding:
    node packages/sdk/scripts/build-bindings.ts native --debug

# Build both platform assets for an SDK distribution on the current supported platform.
package-sdk-bindings:
    node packages/sdk/scripts/build-bindings.ts native
    node packages/sdk/scripts/build-bindings.ts wasm
