#!/bin/sh
# Build the text Orb, then the speller against its contract, show both contracts and run the host.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path text/Cargo.toml
echo
tembra build --manifest-path speller/Cargo.toml
echo
echo "text/contract/text.wit, derived from the component:"
cat text/contract/text.wit
echo
echo "speller/contract/speller.wit, derived from the component:"
cat speller/contract/speller.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- text/artifacts/text.orb.wasm speller/artifacts/speller.orb.wasm
