#!/bin/sh
# Build the filter with `cargo tembra build`, show the derived WIT and run the admission flow.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path filter/Cargo.toml --out artifacts
echo
echo "filter/contract/filter.wit, derived from the component:"
cat filter/contract/filter.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- artifacts/filter.orb.wasm
