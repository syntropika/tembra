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

The implementation uses `satisfies WitMethods<typeof calculation, ReturnType<typeof create>>`
once. TypeScript infers each method's environment, context, arguments and result from the
factory and the WIT contract. Missing or extra methods and incorrect argument or result types
fail the build. Imported Orb types use the canonical WIT identity:

```ts
import type * as Arithmetic from "example:integrated/arithmetic@1.0.0";
```

This is a type-only import. `self.get` acquires the actual provider under the Orb's declared
requirement; neither the type import nor the npm dependency grants host capabilities.

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

Canonical imported interface types work in both the build and the editor without custom
`paths` entries. Repeated builds keep unchanged managed declarations intact.

## Run

```sh
bun examples/wasm-orb/consumer.ts <calculator.wasm> <arithmetic.wasm>
```

The consumer prints `24`: `[3, 4, 5]` sums to 12 and the Rust provider doubles it.
It then prints the stored state and the resource value `42`. The consumer starts a local JSON
API, grants its exact origin, calls `fetchAndSum` inside the TypeScript Wasm Orb and prints
`{ fetched: 120 }` from the API's distinct `[10, 20, 30]` payload. The consumer also checks that
the API received exactly one request. The local API and host close when the example ends.

The calculator artifact declares a WIT import, not a provider. The installer maps
`example:integrated/arithmetic@1.0.0` to the installed `arithmetic` Orb. The
compiled provider needs no interpreter; the calculator lazily loads the packaged
dquickts engine on first use.

Change `orb.ts` and run the build again to produce a new artifact. The engine is
not rebuilt. To replace a running Orb, pass the new bytes with the same
installation choices, for example `host.reload([{ artifact, requires }])`.

## Network access

The `calculator` world imports `wasi:http/client@0.3.0` because its implementation imports the
HTTP helper. Register a host provider and grant exact origins at installation. The included
consumer obtains the origin from its ephemeral local API; applications can select their own:

```ts
const host = createHost({ isolated: { providers: { network: ["outbound"] } } });
await host.load(calculatorFile, {
  requires: { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } },
  grants: { http: [{ provider: "outbound", origins: ["https://api.example.com"] }] },
});
```

TypeScript calls standard WASI HTTP resource, stream and future bindings through the helper.
HTTP framing, DNS and TLS run in the host. Text, bytes and async byte-chunk request sources are
supported. `response.body` streams bytes and `response.events()` parses SSE; `text()` and
`json()` collect at most 1 MiB. Untouched responses can survive callbacks. Close them when they
are no longer needed; an active upload must finish within its callback. The facade accepts
already percent-encoded absolute URLs and does not implement full WHATWG Fetch semantics.

In a browser, select the Fetch adapter through the Tembra facade:

```ts
import { createHost, fetchHttpProvider } from "@syntropika/tembra-sdk/browser";

const host = await createHost({
  isolated: { providers: { http: { outbound: fetchHttpProvider() } } },
});
```

An application-owned `HttpProvider.send` can instead provide raw byte fields, streaming bodies
and trailers. The default Fetch adapter preserves Fetch's platform limits, including buffered
uploads and unavailable trailers.

Browser Fetch retains platform restrictions such as CORS. Redirects are refused
so a request cannot silently leave its granted origin. No browser TCP implementation
is supplied; an embedding can provide an explicit socket adapter. See the
[WASI support matrix](../../crates/executor-wasm/README.md#wasi-coverage) for current limits.
