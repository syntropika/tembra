#!/bin/sh
# Build the harbor with `cargo tembra build`, show the derived WIT and drive the Orb from the host,
# first the compiled Orb from its artifact, then the native Orb the module's `orb()` builds.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path harbor/Cargo.toml
echo
echo "harbor/contract/port.wit, derived from the component:"
cat harbor/contract/port.wit
echo
echo "the compiled Orb:"
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- harbor/artifacts/harbor.orb.wasm
echo
echo "the native Orb:"
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- --native
