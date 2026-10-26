# Build executable adapters before running integration tests.
set positional-arguments

default:
    @just --list

install:
    bun install --frozen-lockfile

build-sdk:
    bun run --cwd packages/ts/sdk build

build-node:
    bun run --cwd packages/ts/engine-node build --debug --features=hooks,subscriptions,views,telemetry

build-wasm:
    bun run --cwd packages/ts/engine-wasm build --features=hooks,subscriptions,views

fmt:
    cargo fmt --all
    bun run format

fmt-check:
    cargo fmt --all --check
    bun run format:check

lint:
    cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
    bun run lint

deps:
    cargo machete

test: build-node build-wasm build-sdk
    cargo test --locked --workspace --all-features
    bun run --cwd packages/ts/sdk test
    bun run --cwd packages/ts/sdk test:node
    bun run --cwd packages/ts/react-native test

check-wasm:
    cargo check --locked -p tembra-core -p tembra-browser-wasm --target wasm32-unknown-unknown --all-features

examples:
    cargo run --locked -p tembra --example counter
    cargo run --locked -p tembra --example addresses
    cargo run --locked -p tembra --example lifetime
    cargo run --locked -p tembra --example executor

check: fmt-check build-node build-wasm build-sdk
    bun run typecheck
    just lint
    just deps
    just test
    just check-wasm
    just examples

install-tools:
    cargo install --locked cargo-machete --version 0.9.2
    cargo install --locked wasm-bindgen-cli --version 0.2.129

install-ts: install
check-all: check

# Generate matching Swift and Kotlin bindings from the shared engine.
build-mobile-bindings:
    bun packages/swift/build.ts bindings
    bun packages/kotlin/build.ts bindings

# Build reusable native packages without React Native.
build-swift:
    bun packages/swift/build.ts build

build-kotlin *args:
    bun packages/kotlin/build.ts jvm {{args}}

build-kotlin-android *args:
    bun packages/kotlin/build.ts android {{args}}

test-swift:
    swift test --package-path packages/swift

test-kotlin: build-kotlin
    gradle -p packages/kotlin :jvm:test
