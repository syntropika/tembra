# Build executable adapters before running integration tests.
set positional-arguments

default:
    @just --list

install:
    bun install --frozen-lockfile

build-sdk:
    bun packages/sdk/scripts/build-sdk.ts

build-node:
    node packages/sdk/scripts/build-bindings.ts native --debug --features=hooks,subscriptions,views,telemetry

build-wasm:
    node packages/sdk/scripts/build-bindings.ts wasm --features=hooks,subscriptions,views

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

test: build-sdk build-node
    cargo test --locked --workspace --all-features
    bun test packages/sdk/test --timeout 15000
    node --test packages/sdk/test/node.test.mjs

check-wasm:
    cargo check --locked -p tembra-core -p tembra-browser-wasm --target wasm32-unknown-unknown --all-features

check: fmt-check build-sdk build-node
    bun run typecheck
    just lint
    just deps
    just test
    just check-wasm

install-tools:
    cargo install --locked cargo-machete --version 0.9.2
    cargo install --locked wasm-bindgen-cli --version 0.2.129

install-ts: install
check-all: check
