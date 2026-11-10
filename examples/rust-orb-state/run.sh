#!/bin/sh
# Build the counter with `cargo tembra build`, show the derived WIT and drive the Orb from the host.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path counter/Cargo.toml
echo
echo "counter/contract/tally.wit, derived from the component:"
cat counter/contract/tally.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- counter/artifacts/counter.orb.wasm
