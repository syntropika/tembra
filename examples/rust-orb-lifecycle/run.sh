#!/bin/sh
# Build the counter component by hand (no macro, no cargo-tembra), then let the host package it
# like a componentized JavaScript Orb and drive it.
set -eu
cd "$(dirname "$0")"
root=../..
nightly=$(sed -n 's/^nightly := "\(.*\)"$/\1/p' "$root/justfile")
cargo +"$nightly" rustc --quiet --locked --release --target wasm32-unknown-unknown \
  --manifest-path counter/Cargo.toml -- -Clinker=wasm-component-ld
component=counter/target/wasm32-unknown-unknown/release/tembra_example_lifecycle_counter.wasm
cargo run --quiet --locked --manifest-path host/Cargo.toml --target-dir "$root/target" -- \
  "$component" counter/contract
