# WIT and TypeScript Orb

This example builds a TypeScript Orb and a compiled Rust Orb from one WIT contract,
then installs both through the ordinary Node host.

| File           | Role                                                                   |
| -------------- | ---------------------------------------------------------------------- |
| `contract.wit` | Canonical contract for both implementations                            |
| `orb.ts`       | Author-owned TypeScript implementation of `calculation`                |
| `package.json` | Pins the `simple-statistics` npm dependency                            |
| `build.ts`     | Calls the SDK `build()` API                                            |
| `consumer.ts`  | Application code that loads the two `.wasm` artifacts and calls an Orb |

`calculation` sums values with `simple-statistics`, calls the imported
`arithmetic` interface, updates managed state and reads a dependent resource.
`fetchAndSum` uses the guest-side `fetch` helper from `@syntropika/tembra-sdk/http`.

## Build

Run from the repository root after `just install` and `just build-node`. Running
the TypeScript Orb also needs the packaged interpreter; see
[DEVELOP.md](../../DEVELOP.md#isolated-orbs).

Compile the Rust provider component. It uses the official `wit-bindgen` macro
against this directory's `contract.wit`:

```sh
cargo +nightly build --locked --release --target wasm32-wasip3 \
  --manifest-path crates/executor-wasm/rust-provider/Cargo.toml
```

Pass the produced component to the build script. Without `--provider`, only the
TypeScript Orb is built:

```sh
bun examples/wasm-orb/build.ts \
  --provider crates/executor-wasm/rust-provider/target/wasm32-wasip3/release/tembra_wasm_arithmetic.wasm
```

`build()` compiles the WIT, generates TypeScript declarations into ignored managed
files (`.tembra/` and `contract.d.wit.ts`), typechecks `orb.ts` and bundles it with
its installed dependencies. The included `tsconfig.json` gives the editor the same guest types. It does not download packages. A compiled component is
checked against the selected world and receives its name and API version from the
build target. Each result is one immutable `orb.<sha256>.wasm` file under
`dist/tembra/<name>/`, or under `--out`.

## Run

```sh
bun examples/wasm-orb/consumer.ts <calculator.wasm> <arithmetic.wasm>
```

The consumer prints `24`: `[3, 4, 5]` sums to 12 and the Rust provider doubles it.
It then prints the stored state and the resource value `42`.

The calculator artifact declares a WIT import, not a provider. The installer maps
`example:integrated/arithmetic@1.0.0` to the installed `arithmetic` Orb. The
compiled provider needs no interpreter; the calculator lazily loads the packaged
dquickts engine on first use.

Change `orb.ts` and run the build again to produce a new artifact. The engine is
not rebuilt. To replace a running Orb, pass the new bytes with the same
installation choices, for example `host.reload([{ artifact, requires }])`.

## Network access

The `calculator` world has no HTTP import, so `fetchAndSum` cannot send requests.
Build with `world: "online-calculator"` to import `wasi:http/client@0.3.0`, then
register a host provider and grant exact origins at installation:

```ts
const host = createHost({ isolated: { providers: { network: ["outbound"] } } });
await host.load(calculatorFile, {
  requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
  grants: { http: [{ provider: "outbound", origins: ["https://api.example.com"] }] },
});
```

The native host uses Wasmtime's WASI HTTP sender for HTTP framing, DNS and TLS.
TypeScript adapts the request and consumes the streamed body; a maintained SSE
parser powers `response.events()`. `text()` and `json()` collect at most 1 MiB.
Ending iteration early closes the response; callback completion and cancellation
also close source response handles. Raw sockets require separate network grants.
The facade does not implement complete WHATWG Fetch semantics.

In a browser, register a Fetch-compatible provider through the same installation
contract. The consumer may replace it with a mock, proxy or another implementation:

```ts
const host = await createHost({
  isolated: { providers: { http: { outbound: globalThis.fetch } } },
});
```

Browser Fetch retains platform restrictions such as CORS. Redirects are refused
so a request cannot silently leave its granted origin. No browser TCP implementation
is supplied; an embedding can provide an explicit socket adapter. See the
[WASI support matrix](../../crates/executor-wasm/README.md#wasi-coverage) for current limits.
