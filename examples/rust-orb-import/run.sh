#!/bin/sh
# Build the greeter, then the welcomer against its contract, show both contracts and run the host.
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
tembra build --manifest-path welcomer/Cargo.toml
echo
echo "greeter/contract/greeter.wit, derived from the component:"
cat greeter/contract/greeter.wit
echo
echo "welcomer/contract/welcomer.wit, derived from the component:"
cat welcomer/contract/welcomer.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- greeter/artifacts/greeter.orb.wasm welcomer/artifacts/welcomer.orb.wasm
