# Executor source fixtures

Each directory holds one SDK-authored source Orb (`orb.ts`) and its public WIT (`contract.wit`)
with exactly one world.
The fixture build (`scripts/test-wasm-fixtures.ts`) compiles every directory with the SDK build
into `$TEMBRA_WASM_FIXTURES/<directory>.wasm`. The executor tests load only those prepared
artifacts; no interpreter or source text crosses into Rust.
