#!/bin/sh
# Build the gate and the blocker with `cargo tembra build`, show their contracts and run three admissions.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path gate/Cargo.toml
echo
tembra build --manifest-path blocker/Cargo.toml
echo
echo "gate/contract/gate.wit, derived from the component:"
cat gate/contract/gate.wit
echo
echo "blocker/contract/blocker.wit, derived from the component:"
cat blocker/contract/blocker.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- gate/artifacts/gate.orb.wasm blocker/artifacts/blocker.orb.wasm
