#!/bin/sh
# Build the decoder, then the player against the decoder's contract, show both contracts and run the host.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path decoder/Cargo.toml
echo
tembra build --manifest-path player/Cargo.toml
echo
echo "decoder/contract/decoder.wit, derived from the component:"
cat decoder/contract/decoder.wit
echo
echo "player/contract/player.wit, derived from the component:"
cat player/contract/player.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- decoder/artifacts/decoder.orb.wasm player/artifacts/player.orb.wasm
