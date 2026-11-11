# Build executable adapters before running integration tests.
set positional-arguments

# Pinned nightly for the interpreter core module and compiled Wasm fixtures; CI reads this value.
nightly := "nightly-2026-10-15"

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
    just test-no-hooks
    just test-native-rpc
    bun test scripts/release.test.ts
    bun run --cwd packages/ts/sdk test
    bun run --cwd packages/ts/sdk test:node
    bun run --cwd packages/ts/react-native test

# Verify the core and bridge with optional hook APIs disabled.
test-no-hooks:
    cargo test --locked -p tembra-core -p tembra-bridge --no-default-features

# Execute documented Rust guarantees, including the optional native hooks facade.
test-doc:
    cargo test --locked --workspace --all-features --doc

# Build the reusable dquickts interpreter and package it into the engine packages; Orb edits reuse it.
# Without a dquickts checkout, the build clones one into the cache and still verifies the pinned revision.
build-interpreter sdk cache source="" toolchain=nightly:
    bun crates/executor-wasm/guest/build.ts --sdk "$1" --cache "$2" ${3:+--source "$3"} --toolchain "$4" --package

# Compile the example Rust provider component; reuse the target directory between runs.
build-provider target-dir toolchain=nightly:
    cargo +"$2" build --locked --release --target wasm32-wasip3 --manifest-path crates/executor-wasm/rust-provider/Cargo.toml --target-dir "$1"

# Compile the controlled component used to qualify settlement, quarantine and filesystem grants.
build-probe target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/probe-provider/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile a component that imports synchronous and asynchronous Orb interfaces.
build-consumer target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-consumer/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile an Orb selecting root-owned instances through typed WIT reference views.
build-keyed-router target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-keyed-router/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile the resource provider and its independent importer against the same WIT contract.
build-resources target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-resource-provider/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-resource-consumer/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile the byte-stream components used by native portable transport qualification.
build-streams target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-stream-producer/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-stream-consumer/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile a component contributing to waterfall, series and bail hooks of another Orb.
build-contributor target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-contributor/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile a component using standard WASI HTTP request, response and body resources.
build-http-client target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-http-client/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile a component using standard WASI sockets: TCP, UDP, listen/accept and name lookup.
build-sockets target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-sockets/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile the complete clocks, random and CLI operation probe.
build-system target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-system/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile the descriptor and directory operation probe.
build-filesystem target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-filesystem/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Compile the incoming HTTP handler fixture: its handler-only world into "$1/handler" and its proxy
# world, which also imports the outgoing client, into "$1/proxy".
build-http-handler target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-http-handler/Cargo.toml --target-dir "$1/handler" -- -Clinker=wasm-component-ld
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/rust-http-handler/Cargo.toml --target-dir "$1/proxy" --features proxy -- -Clinker=wasm-component-ld

# Rebuild the private source-path stream adapter the executor embeds (64 KiB shadow stack).
build-stream-adapter target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/stream-adapter/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld -Clink-arg=-zstack-size=65536
    cp "$1/wasm32-unknown-unknown/release/tembra_stream_adapter.wasm" crates/executor-wasm/src/worker/standard/stream_adapter.wasm

# Verify the embedded stream adapter is byte-identical to a fresh build of its source.
check-stream-adapter target-dir toolchain=nightly:
    cargo +"$2" rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/executor-wasm/stream-adapter/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld -Clink-arg=-zstack-size=65536
    cmp "$1/wasm32-unknown-unknown/release/tembra_stream_adapter.wasm" crates/executor-wasm/src/worker/standard/stream_adapter.wasm

# Execute isolated Orb acceptance on native Rust, Node and Chrome after the packaged interpreter build.
test-wasm-acceptance *args:
    bun scripts/test-wasm-acceptance.ts "$@"

# Verify native hook lifecycle contracts across the core, facade and Wasm executor.
test-hook-lifecycle: build-node build-wasm build-sdk
    cargo test --locked -p tembra-core -p tembra-runtime -p tembra -p tembra-executor-wasm --all-features

# Exchange actual SDK RPC frames with a native Rust provider over application-owned bytes.
test-native-rpc:
    cargo build --locked -p tembra --features rpc,views --example rpc
    TEMBRA_NATIVE_RPC="$(realpath "${CARGO_TARGET_DIR:-target}")/debug/examples/rpc" bun test packages/ts/sdk/test/rpc-native.test.ts

check-wasm:
    cargo check --locked -p tembra-core -p tembra-browser-wasm --target wasm32-unknown-unknown --all-features

examples:
    cargo run --locked -p tembra --example counter
    cargo run --locked -p tembra --example addresses
    cargo run --locked -p tembra --example lifetime
    cargo run --locked -p tembra --example executor
    cargo run --locked -p tembra --features hooks --example hooks
    cargo run --locked -p tembra --example composition

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

# Exercise the portable mobile transport with real source and compiled guests.
test-mobile-wasm engine provider probe handler keyed-router:
    TEMBRA_WASM_ENGINE="{{absolute_path(engine)}}" TEMBRA_WASM_PROVIDER="{{absolute_path(provider)}}" TEMBRA_WASM_PROBE="{{absolute_path(probe)}}" TEMBRA_WASM_HTTP_HANDLER="{{absolute_path(handler)}}" TEMBRA_WASM_KEYED_ROUTER="{{absolute_path(keyed-router)}}" cargo test --locked -p tembra-mobile --test execution --test compiled_filesystem -- --ignored
