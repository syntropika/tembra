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
    just test-native-fixture
    just test-native-rpc
    bun test scripts/release.test.ts
    bun run --cwd packages/ts/sdk test
    bun run --cwd packages/ts/sdk test:node
    bun run --cwd packages/ts/react-native test

# Verify the core and bridge with optional hook APIs disabled.
test-no-hooks:
    cargo test --locked -p tembra-core -p tembra-bridge --no-default-features

# Verify the macro's native fixture without its `archive` feature, whose `#[cfg]` items then leave its WIT and `orb(..)`.
test-native-fixture:
    cargo test --locked -p tembra-orb-native-fixture

# Run the `cargo tembra` commands end to end on scratch crates: `new`, `build --out`, `wit` and `inspect`. The crates build as components and natively.
test-cargo-tembra:
    cargo test --locked -p cargo-tembra --test commands -- --ignored

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

# Build and run every #[orb] example; the contracts and lockfiles committed beside them must not change.
test-orb-examples:
    #!/usr/bin/env bash
    set -euo pipefail
    for script in examples/rust-orb-*/run.sh; do
        echo "== $script"
        "$script"
    done
    git diff --exit-code -- examples

# Build the macro's async and std/no_std guests into one reusable target directory.
build-orb-macro target-dir:
    #!/usr/bin/env bash
    set -euo pipefail
    fixture_target="$1"
    mkdir -p "$fixture_target"
    fixture_target="$(cd "$fixture_target" && pwd)"
    cargo rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/orb-macro/async-fixture/Cargo.toml --target-dir "$fixture_target" -- -Clinker=wasm-component-ld
    cargo rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/orb-macro/runtime-fixture/Cargo.toml --target-dir "$fixture_target" -- -Clinker=wasm-component-ld
    cp "$fixture_target/wasm32-unknown-unknown/release/tembra_orb_runtime_fixture.wasm" "$fixture_target/std.wasm"
    cargo rustc --locked --release --target wasm32-unknown-unknown --manifest-path crates/orb-macro/runtime-fixture/Cargo.toml --target-dir "$fixture_target" --features no-std -- -Clinker=wasm-component-ld
    cp "$fixture_target/wasm32-unknown-unknown/release/tembra_orb_runtime_fixture.wasm" "$fixture_target/no-std.wasm"

# Execute real macro component boundaries; acceptance uses the same fixture builder.
test-orb-macro:
    #!/usr/bin/env bash
    set -euo pipefail
    fixture_target="${CARGO_TARGET_DIR:-target}/orb-macro"
    just build-orb-macro "$fixture_target"
    fixture_target="$(cd "$fixture_target" && pwd)"
    TEMBRA_WASM_ORB_MACRO_ASYNC="$fixture_target/wasm32-unknown-unknown/release/tembra_orb_macro_async_fixture.wasm" \
    TEMBRA_WASM_ORB_MACRO_STD="$fixture_target/std.wasm" \
    TEMBRA_WASM_ORB_MACRO_NO_STD="$fixture_target/no-std.wasm" \
        cargo test --locked -p tembra-executor-wasm --test orb_macro_async --test orb_macro_runtime -- --ignored

# Build the parity fixture's Orb as a component into one reusable target directory, with the overflow checks of its native tests.
build-orb-parity target-dir:
    CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true cargo rustc --locked --release --target wasm32-unknown-unknown --crate-type cdylib --manifest-path crates/orb-macro/parity-fixture/Cargo.toml --target-dir "$1" -- -Clinker=wasm-component-ld

# Require the same answers from the parity fixture's Orb built natively and as a component; acceptance uses the same builder.
test-orb-parity:
    #!/usr/bin/env bash
    set -euo pipefail
    fixture_target="${CARGO_TARGET_DIR:-target}/orb-parity"
    just build-orb-parity "$fixture_target"
    fixture_target="$(cd "$fixture_target" && pwd)"
    TEMBRA_WASM_ORB_PARITY="$fixture_target/wasm32-unknown-unknown/release/tembra_orb_parity_fixture.wasm" \
        cargo test --locked -p tembra-executor-wasm --test orb_macro_parity -- --ignored

# Build with Cargo the fixtures of crates/orb-macro/config-fixtures and the facade's features; a failing test names its configuration.
test-configurations:
    cargo test --locked -p tembra-orb-macro --test configurations -- --ignored

# Build each Orb crate of the examples and of the macro's fixtures the way the other recipes do not: natively, or, the native fixture, checked for wasm32. Every step runs; the recipe then fails, naming each step that failed.
test-orb-both-ways:
    #!/usr/bin/env bash
    set -euo pipefail
    target="${CARGO_TARGET_DIR:-target}/orb-both-ways"
    hosts=crates/orb-macro/config-fixtures/native-hosts
    native_hosts="tembra-config-native-host tembra-config-native-hooks-host"
    failed=""
    # Run the command after the step's name, and note the step when it fails.
    step() {
        echo "== $1"
        if ! "${@:2}"; then
            failed+="  $1"$'\n'
        fi
    }
    # Natively, the Orb crates of the examples, as the fixture's native hosts link them: with the
    # facade's `host`, and `hooks` where a module contributes to a hook. Each host builds on its
    # own, so that Cargo unifies the features of its Orb crates only, and names every crate that
    # fails.
    for host in $native_hosts; do
        step "$host" cargo build --locked --keep-going --manifest-path "$hosts/Cargo.toml" -p "$host" --target-dir "$target"
    done
    # The native hosts link each Orb crate of the examples but those native already: the hosts, the
    # hook's provider written by hand, and the Orb crates the native example's host links. Cargo
    # lists the hosts' normal dependencies, which a commented-out entry or a dev-dependency is not.
    each_crate_linked() {
        local linked crate status=0
        linked="$(for host in $native_hosts; do
            cargo tree --locked --manifest-path "$hosts/Cargo.toml" -p "$host" -e normal --depth 1 --prefix none --format '{p}' || exit 1
        done)" || return 1
        for manifest in examples/rust-orb-*/*/Cargo.toml; do
            crate="${manifest%/Cargo.toml}"
            case "$crate" in
                */host | examples/rust-orb-hook-contribution/gate | examples/rust-orb-native/*) ;;
                *)
                    if ! grep -qF "($(pwd -P)/$crate)" <<< "$linked"; then
                        echo "No native host of $hosts links $crate" >&2
                        status=1
                    fi
                    ;;
            esac
        done
        return "$status"
    }
    step "a native host links each Orb crate of the examples" each_crate_linked
    # Natively, the async and host feature fixtures, whose `host` feature puts the runtime in their
    # lockfiles. Not the parity fixture, which `just test-orb-parity` builds both ways.
    for fixture in async-fixture host-feature-fixture; do
        step "crates/orb-macro/$fixture natively" cargo build --locked --manifest-path "crates/orb-macro/$fixture/Cargo.toml" --features host --target-dir "$target"
    done
    # Natively, the runtime fixture's default module `guest`, std and `no-std`, with the facade's
    # `host` through the fixture's `native` feature. `just test-orb-macro` builds `guest` only as a
    # component. It builds natively, std, and tests each module that a feature selects in place of
    # `guest`, but `flagged`, whose native test the native fixture's `tests/flags.rs` runs.
    for features in native native,no-std; do
        step "crates/orb-macro/runtime-fixture natively, with $features" cargo build --locked --manifest-path crates/orb-macro/runtime-fixture/Cargo.toml --features "$features" --target-dir "$target"
    done
    # For wasm32, the native fixture, which its tests build natively. Checked, not built: a
    # component holds one Orb, so a build of a crate with several fails on the second, which
    # `just test-configurations` asserts on `crates/orb-macro/config-fixtures/two-orbs`.
    step "crates/orb-macro/native-fixture checked for wasm32" cargo check --locked -p tembra-orb-native-fixture --target wasm32-unknown-unknown --target-dir "$target"
    if [ -n "$failed" ]; then
        printf 'These steps failed:\n%s' "$failed" >&2
        exit 1
    fi

check: fmt-check check-ci-parts build-node build-wasm build-sdk
    bun run typecheck
    just lint
    just deps
    just test
    just check-wasm
    just examples

install-tools:
    cargo install --locked cargo-machete --version 0.9.2
    cargo install --locked wasm-bindgen-cli --version 0.2.129
    cargo install --locked wasm-component-ld --version 0.5.31

install-ts: install
check-all: check

# Run every part of the verification in order, and fail when the parts change the tree; CI runs each part in a job of its own.
ci: _record-tree \
    check \
    test-orb-examples \
    test-orb-macro \
    test-cargo-tembra \
    test-orb-parity \
    test-configurations \
    test-orb-both-ways \
    && _compare-tree

# Fail when the parts of `ci` and the recipes of the job matrix in the quality workflow differ.
check-ci-parts:
    #!/usr/bin/env bash
    set -euo pipefail
    # The parts are the recipes `ci` runs before its own steps, except the helpers whose names start
    # with an underscore. The matrix lists each part as `- { recipe: <name>, timeout: <minutes> }`.
    parts="$(just --show ci | sed -n 's/^ci: \(.*\) && .*/\1/p' | tr ' ' '\n' | grep -v '^_' | sort || true)"
    jobs="$(sed -n 's/^ *- { recipe: \([a-z0-9-]*\),.*/\1/p' .github/workflows/quality.yml | sort)"
    only_ci="$(comm -23 <(echo "$parts") <(echo "$jobs") | paste -sd ' ' -)"
    only_jobs="$(comm -13 <(echo "$parts") <(echo "$jobs") | paste -sd ' ' -)"
    if [ -z "$parts" ] || [ -z "$jobs" ] || [ -n "$only_ci$only_jobs" ]; then
        echo "The parts of \`just ci\` and the job matrix of .github/workflows/quality.yml differ:" >&2
        echo "  only in ci: ${only_ci:-none}" >&2
        echo "  only in the matrix: ${only_jobs:-none}" >&2
        exit 1
    fi

# Fail on any modified or deleted file, and on any new file that .gitignore does not list; CI runs it after each part, on a clean checkout.
check-clean-tree:
    #!/usr/bin/env bash
    set -euo pipefail
    # Show new files whatever `status.showUntrackedFiles` says.
    changes="$(git status --porcelain --untracked-files=normal)"
    if [ -n "$changes" ]; then
        echo "The checks changed the tree:" >&2
        echo "$changes" >&2
        exit 1
    fi

# Print the id of a Git tree that holds the working tree as it is: the tracked files with their changes, and the new files that .gitignore does not list, but no nested repository that HEAD does not record.
_tree:
    #!/usr/bin/env bash
    set -euo pipefail
    # An index of its own keeps the real index as it is.
    scratch="$(mktemp -d)"
    trap 'rm -rf "$scratch"' EXIT
    export GIT_INDEX_FILE="$scratch/index"
    git read-tree HEAD
    # Git warns about each nested repository it adds, and the next step takes them out again.
    git add --all 2> "$scratch/warnings" || { cat "$scratch/warnings" >&2; exit 1; }
    # A nested repository, or a linked worktree, in a directory that .gitignore does not list is added
    # as a gitlink at its current commit, which another process may move while the parts run. Leave out
    # the gitlinks that HEAD does not have.
    git ls-files --stage -z | while IFS= read -r -d '' entry; do
        path="${entry#*$'\t'}"
        if [ "${entry%% *}" = 160000 ] && [ "$(git ls-tree HEAD -- "$path" | cut -c1-6)" != 160000 ]; then
            printf '%s\0' "$path"
        fi
    done | git update-index --force-remove -z --stdin
    git write-tree

# Record the working tree before the parts of `ci` run.
_record-tree:
    #!/usr/bin/env bash
    set -euo pipefail
    just _tree > "$(git rev-parse --git-path tembra-ci-tree)"

# Fail when the working tree is not the one that `_record-tree` recorded, and name the files that differ.
_compare-tree:
    #!/usr/bin/env bash
    set -euo pipefail
    record="$(git rev-parse --git-path tembra-ci-tree)"
    before="$(cat "$record")"
    rm -f "$record"
    after="$(just _tree)"
    if [ "$before" != "$after" ]; then
        echo "The parts changed the tree (A added, M modified, D deleted):" >&2
        git diff-tree -r --name-status "$before" "$after" >&2
        exit 1
    fi

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
