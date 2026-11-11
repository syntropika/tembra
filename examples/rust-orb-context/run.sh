#!/bin/sh
# Build the audit with `cargo tembra build`, show the derived WIT and drive the Orb from the host.
set -eu
cd "$(dirname "$0")"
root=../..
tembra() {
  if command -v cargo-tembra > /dev/null 2>&1; then cargo tembra "$@"; else
    cargo run --quiet --locked --manifest-path "$root/Cargo.toml" -p cargo-tembra -- "$@"
  fi
}
tembra build --manifest-path audit/Cargo.toml
echo
echo "audit/contract/audit.wit, derived from the component:"
cat audit/contract/audit.wit
echo
cargo run --quiet --manifest-path host/Cargo.toml --target-dir "$root/target" -- audit/artifacts/audit.orb.wasm
