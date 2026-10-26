# Build executable adapters before running integration tests.
set positional-arguments

default:
    @just --list

install:
    bun install --cwd packages/ts --frozen-lockfile

build-sdk:
    bun run --cwd packages/ts/sdk build

build-node:
    bun run --cwd packages/ts/engine-node build --debug --features=hooks,subscriptions,views,telemetry

build-wasm:
    bun run --cwd packages/ts/engine-wasm build --features=hooks,subscriptions,views

fmt:
    cargo fmt --all
    bun run --cwd packages/ts format

fmt-check:
    cargo fmt --all --check
    bun run --cwd packages/ts format:check

lint:
    cargo clippy --locked --workspace --all-features --all-targets -- -D warnings
    bun run --cwd packages/ts lint

deps:
    cargo machete

test: build-node build-wasm build-sdk
    cargo test --locked --workspace --all-features
    bun run --cwd packages/ts/sdk test
    bun run --cwd packages/ts/sdk test:node

check-wasm:
    cargo check --locked -p tembra-core -p tembra-browser-wasm --target wasm32-unknown-unknown --all-features

examples:
    cargo run --locked -p tembra --example counter
    cargo run --locked -p tembra --example addresses
    cargo run --locked -p tembra --example lifetime
    cargo run --locked -p tembra --example executor

check: fmt-check build-node build-wasm build-sdk
    bun run --cwd packages/ts typecheck
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
