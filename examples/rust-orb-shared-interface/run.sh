#!/bin/sh
# Build the compiled keeper with `cargo tembra build`, show its contract and consult both keepers.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path book/Cargo.toml
echo
echo "book/contract/keeper.wit, derived from the component:"
cat book/contract/keeper.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- book/artifacts/book.orb.wasm
