# Spike: typed state over WIT versus JSON

A throwaway measurement for #13, not for merging. It answers one question: what does a compiled
Orb's state cost when it crosses the component boundary as a WIT record instead of JSON text, on
the native worker and on the JavaScript hosts?

## What runs

`wit/spike.wit` declares a ledger state (`u64` fields past 2^53, a string, a list of entries with
a string list each) and three state imports. The guest (`guest/`) runs the same loop on each,
`iterations` times inside one export call: read the state, add 1 to the balance, update it, and
decode the value `update` answers, as the macro's generated code does.

| encoding | guest | host |
|---|---|---|
| JSON today | serde, Rust names, 64-bit integers as JSON numbers | parses the text into the kernel's JSON and prints it back |
| JSON exact | serde with `rename_all = "camelCase"` and 64-bit integers as decimal strings | as above |
| WIT record | wit-bindgen's record | lifts the record and converts it to the kernel's JSON (64-bit integers as strings), and back |

Each host also measures a reference that tembra cannot use as is:

- native (`host-native/`, wasmtime `=51.0.0-rc.1`): the WIT record path uses the dynamic `Val`
  API, as `crates/executor-wasm/src/component/worker.rs` does, because the worker cannot know
  each Orb's state type at compile time. The reference uses static `bindgen!` bindings and keeps
  the Rust value: the cost of the canonical ABI alone.
- JavaScript (`host-js/`, `@bytecodealliance/jco-transpile` 0.23.0, the SDK's version, Node 24):
  the WIT record path converts jco's object (`bigint` fields) to a JSON-safe value by walking the
  WIT type, as the SDK's value conversion does. The reference keeps jco's object as it is: the cost
  of jco's generated lifting and lowering alone. The SDK runs exports under JSPI; the spike calls
  one export around the whole loop, so it instantiates without JSPI.

The kernel's own hop (the worker's channel, the JS `execution.sync`) is the same for every
encoding and is left out.

## Results

Median of five runs, each on a fresh instance after a warm-up, microseconds per read and update.
The machine was shared, so differences under about 20% are noise.

### Native (wasmtime)

| entries | JSON today | JSON exact | WIT record, dynamic `Val` | WIT record, static bindings (reference) |
|---:|---:|---:|---:|---:|
| 3 | 8.4 | 9.5 | 13.8 | 4.1 |
| 100 | 223 | 261 | 390 | 153 |
| 1000 | 2618 | 2206 | 3662 | 1318 |

### JavaScript (jco)

| entries | JSON today | JSON exact | WIT record, jco + conversion | WIT record, jco only (reference) |
|---:|---:|---:|---:|---:|
| 3 | 49 | 53 | 77 | 72 |
| 100 | 349 | 297 | 1157 | 991 |
| 1000 | 2870 | 2534 | 11531 | 9776 |

## Reading

- On JavaScript hosts the WIT record is about 1.5 times slower than JSON for a small state and 3
  to 4 times slower from about a hundred entries. The cost is jco's generated code: without any
  conversion it stays within 20% of the converted path. `JSON.parse` and `JSON.stringify` are
  native code; jco lifts and lowers every field, string and list element in JavaScript.
- On the native worker the WIT record is about 1.4 to 1.7 times slower than JSON, because of the
  dynamic `Val` tree and its conversion to the kernel's JSON. With static bindings the canonical
  ABI is faster than JSON, but the worker serves every Orb's state with one code path, so it
  cannot have them.
- JSON with camelCase names and 64-bit integers as strings costs the same as JSON today, within
  the noise. It fixes both problems #13 names (precision past 2^53 in JavaScript, Rust field
  names) without changing `tembra:guest/state`.

## Run it

```sh
cd spikes/typed-state
(cd guest && cargo rustc --release --target wasm32-unknown-unknown --target-dir ../target -- -Clinker=wasm-component-ld)
(cd host-native && cargo build --release --target-dir ../target)
(cd host-js && npm install)
./target/release/spike-typed-state-host target/wasm32-unknown-unknown/release/spike_typed_state_guest.wasm
node host-js/bench.mjs target/wasm32-unknown-unknown/release/spike_typed_state_guest.wasm
```
