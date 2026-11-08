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

`build()` compiles the WIT, generates Jco type declarations into ignored managed
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

The `calculator` world has no socket import, so `fetchAndSum` cannot connect.
Build with `world: "online-calculator"` to import WASI sockets, then register a
network provider and grant exact destinations at installation:

```ts
const host = createHost({ isolated: { providers: { network: ["outbound"] } } });
await host.load(calculatorFile, {
  requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
  grants: { network: [{ provider: "outbound", connect: ["127.0.0.1:8080"] }] },
});
```

The helper frames HTTP/1 in TypeScript over the granted TCP connection. It supports
bounded UTF-8 `GET` requests to literal-IP `http:` URLs only. It is not WASI HTTP
or WHATWG Fetch: TLS, DNS, redirects and streaming are not supported.
