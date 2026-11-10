#!/bin/sh
# Build the greeter with `cargo tembra build`, show the derived WIT and call the Orb from the host.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path greeter/Cargo.toml
echo
echo "greeter/contract/greeter.wit, derived from the component:"
cat greeter/contract/greeter.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- greeter/artifacts/greeter.orb.wasm
