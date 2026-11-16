#!/bin/sh
# Build both greeters as components, show their contracts, then run the native host linking them.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path english/Cargo.toml
echo
tembra build --manifest-path italian/Cargo.toml
echo
echo "english/contract/english.wit, derived from the component:"
cat english/contract/english.wit
echo
echo "italian/contract/italian.wit, derived from the component:"
cat italian/contract/italian.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target"
