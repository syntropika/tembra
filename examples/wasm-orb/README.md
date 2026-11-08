# WIT and TypeScript Orb

The author-owned implementation is `orb.ts`, its contract is `contract.wit`, and
`package.json` pins its npm dependency. Official WIT types are generated in memory
by Tembra tooling. There is no checked-in bindings directory or hand-written API
implementation generator.

`calculation` exports npm-backed summation, managed state, dependent resources,
error/recovery scenarios and an HTTP-backed calculation. The installed provider
for the imported `arithmetic` interface is selected by the host. Its WIT package
version identifies a contract; the Orb's installation identity and API version
remain separate. The `online-calculator` world adds the standard WASI socket
import; its installer must separately grant exact TCP destinations.

The implementation imports the guest-side `fetch` helper from
`@syntropika/tembra-sdk/http`. The TypeScript code frames HTTP, interprets status
and headers, parses JSON and performs the calculation inside Wasm. The helper
currently supports bounded UTF-8 HTTP/1 GET requests to literal-IP `http:` URLs;
see its profile before using it as a general-purpose client.

`consumer.mjs` is an application-side example. It installs both TypeScript and
compiled Rust artifacts through the same SDK facade and closes their Stores.

The complete preparation, native consumer and real integration commands are in
[the executor guide](../../crates/executor-wasm/README.md). The native consumer
needs only the reusable engine, one prepared JSON artifact and the Rust provider component. JavaScript logic
changes require preparing source again, without rebuilding the Wasm engine.
