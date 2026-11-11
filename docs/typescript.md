# TypeScript

The SDK declares JS orbs and runs their callbacks.
Rust checks ownership and lifecycle transitions.
Application values stay in the JS runtime until a language or transport boundary needs a codec.

## Run an orb in Node

```ts
import {
  orb,
  type Context,
  type Declaration,
  type ServiceDefinition,
  type State,
} from "@syntropika/tembra-sdk";
import { createHost } from "@syntropika/tembra-sdk/node";

type Counter = { count: State<number> };

const counterService = {
  create(self: Declaration) {
    return { count: self.state(0, { validate: Number.isSafeInteger }) };
  },
  methods: {
    increment({ count }: Counter, ctx: Context) {
      return count.update(ctx, (value) => value + 1);
    },
  },
} satisfies ServiceDefinition;

const counter = orb({
  name: "counter",
  apiVersion: 1,
  services: { counter: counterService },
});

const host = createHost();
try {
  await host.install(counter);
  const reference = await host.get(counterService, "main");
  console.log(await reference.increment()); // 1
  reference.release();
} finally {
  await host.close();
}
```

Factories finish synchronously. Methods and resource callbacks can be asynchronous.
`host.get()` waits until the object is ready.
A method call without an explicit context starts its own root.
Pass an existing context as the last argument to join that operation.
Returning a context or copying an opaque token does not keep its issuing call alive.
See [context lifetime](context.md) before sharing a binding beyond a method return.

Passing a service definition infers the reference's methods, business arguments and asynchronous
results. The implementation's environment and Context parameters are removed from public calls.
The returned `count` State handle also makes `reference.state.value` a number. A factory may return
the State handle directly or as a property of its environment. If its State is declared only as a
side effect, its type remains unknown; handler environments and opaque resources are not inferred.

`host.hold(service, key)`, `ctx.get(service, key)` and `self.get(service, key)` accept the same
definitions. Held references expose `ready`; Context references end with their root; declared
dependencies still require the owner's existing `requires` and method permissions.

`orb()` associates original service objects and its frozen copies with their registered names and
API versions. Declare the Orb before acquiring a service. One service object must not be reused
under a different orb, name or API version; create a fresh definition for that identity. Compatible
reloads continue to dispatch through current managed references rather than captured handlers.

Passing a service name string still returns a dynamic reference. This supports discovered services,
isolated orbs, native implementations and explicit RPC boundaries without importing executable
definitions. The identity association is not serialized or shared across workers, processes or
isolated orbs. Keep runtime validation in the orb even when application code supplies TypeScript types.

## Isolated orbs

Orbs declared with `orb()` in application code are trusted. Their callbacks run in the host's
JavaScript runtime. To run an orb isolated in WebAssembly, [build](tooling.md) it into a `.wasm`
artifact and install that artifact:

```ts
import { readFile } from "node:fs/promises";
import { createHost, directoryFilesystem } from "@syntropika/tembra-sdk/node";

const host = createHost({
  isolated: {
    providers: { filesystem: { data: directoryFilesystem("./data") }, network: ["outbound"] },
  },
});
const requires = { "example:integrated/arithmetic@1.0.0": { orb: "arithmetic", apiVersion: 1 } };

await host.load("dist/tembra/arithmetic/orb.<sha256>.wasm");
await host.load("dist/tembra/calculator/orb.<sha256>.wasm", { requires });

// Install, prepare and reload also accept artifact bytes directly.
const artifact = new Uint8Array(await readFile("revised.wasm"));
await host.reload([{ artifact, requires }]);
```

The default `host.load(location, { requires, grants, cli })` loader accepts only `.wasm` artifacts.
It reads `file:` locations and fetches other URLs. The artifact supplies
the orb name, API version and WIT contract. The installer supplies the rest:

- `requires` maps each WIT import to an installed orb and API version.
- `grants` gives this orb access to registered providers. Providers alone grant nothing.
- `cli` supplies explicit arguments, environment, initial directory and standard byte input/output.

```ts
grants: {
  filesystem: [{ provider: "data", mount: "/data", writable: false }],
  network: [{ provider: "outbound", connect: ["127.0.0.1:8080"] }],
}
```

Installation is refused when the orb imports a WASI interface without a matching grant.
Source and compiled native Orbs use standard WASI filesystem descriptors within granted
preopens. Source bindings support binary reads, streamed writes and appends, metadata, directory
streams and mutations. The `readBytes` and `readText` conveniences collect at most 16 MiB by
default; direct streams do not collect the whole file. `writeBytes` and `writeText` use the same
standard byte streams. Filesystem grants do not authorize HTTP or sockets.

The helper in `@syntropika/tembra-sdk/http` uses generated standard `wasi:http` resources.
Declare `import wasi:http/client@0.3.0` in the Orb's WIT; TypeScript imports of the helper use
that contract automatically. HTTP framing, DNS and TLS execute in the host. Request bodies
can be text, bytes or an `AsyncIterable<Uint8Array>` and stream in bounded chunks. The same
standard error/future boundary prevents a failed producer from committing a truncated upload.

Consume `response.body` for bytes, `response.events()` for SSE, or `text()`/`json()` to collect
up to 1 MiB. `headers.getAll(name)` and header iteration preserve raw duplicate fields where the
host exposes them. `await response.trailers()` reads trailers after the body. Untouched responses
can survive a callback boundary; call `close()` when a response is no longer needed. Iteration
closes its active body on EOF or early return. Browser Fetch retains its platform restrictions;
an injected host transport supplies the implementation. The convenience is not a complete
WHATWG Fetch implementation. The host's `timeoutMs` bounds an active callback, so configure it
for the expected duration of an SSE operation. A streamed upload and its trailers must finish
within the callback that starts them; callback end abandons unfinished producers. Pass an already
percent-encoded absolute URL. Request `timeouts` are nanoseconds; a transport may refuse an
unsupported timeout. Standard failures throw `HttpError`, whose `code` preserves the HTTP,
header or request-options error, or is `undefined` for a payload-free request refusal.

For browser hosts, select the Fetch adapter through Tembra:

```ts
import { createHost, fetchHttpProvider } from "@syntropika/tembra-sdk/browser";

const host = await createHost({
  isolated: { providers: { http: { outbound: fetchHttpProvider() } } },
});
await host.load("online.wasm", {
  grants: { http: [{ provider: "outbound", origins: ["https://api.example.com"] }] },
});
```

The default Fetch adapter buffers request bodies up to 8 MiB, combines duplicate response
headers and cannot supply trailers or a separate connect timeout. An application-owned
`HttpProvider.send` can expose raw byte fields, pull-based request and response streams and
trailers. Its request includes the validated raw `pathWithQuery`; `url` is the normalized URL
used by Fetch. Tembra checks the Orb's origin grant before invoking either implementation.
Providers, including mocks, must consume or cancel each request body. Returning a canned
response while ignoring an upload leaves `text()`/`json()` waiting for that upload until the
callback timeout. A clean response EOF permits a duplex upload to continue; it does not cancel it.

Source Orbs import standard interfaces by WIT name: `wasi:random/{random,insecure,insecure-seed}`,
`wasi:clocks/{system-clock,monotonic-clock}`, `wasi:cli` interfaces and
`wasi:filesystem/{preopens,types}@0.3.0`.
Values follow the generated jco declarations: 64-bit integers are `bigint`, `list<u8>` is a
`Uint8Array`, and refused filesystem calls throw an error whose `payload` is the WASI
`error-code`. Descriptors belong to their Orb revision and may survive callbacks. Use `using`
for deterministic disposal; collection provides best-effort cleanup at safe boundaries, and
revision retirement closes remaining descriptors. Cancelling a callback settles its pending
calls without revoking a descriptor retained by that revision.

Filesystem timestamps do not grant access to clock functions. Type-only WIT dependencies stay
available for TypeScript imports such as `import type { Instant } from "wasi:clocks/system-clock@0.3.0"`;
runtime imports need callable authority in the world and the matching installation grant.

Source builtins also respect system grants: reading the current time with `Date`, `Date.now`
or `performance.now` requires `"clocks"`; `Math.random` requires `"random"`. Pure date construction,
parsing and arithmetic remain available. `Math.random` is deliberately insecure; use standard
`wasi:random/random` for cryptographic bytes. Host deadlines use the host clock independently.
Without the clocks grant, `performance.timeOrigin` reports the fixed zero used by the VM's
libc clock; it does not reveal the host's time.

Without injected providers, CLI configuration defaults to empty arguments/environment/input and discarded output. In Node,
select bytes or native files explicitly:

```ts
await host.load("tool.wasm", {
  grants: { system: ["cli"] },
  cli: {
    arguments: ["--inspect"],
    environment: [["MODE", "test"]],
    stdin: { bytes: new Uint8Array([0, 255, 128]) },
    stdout: { file: "./output.bin" }, // Appends bytes to a regular file; creates it if absent.
  },
});
```

CLI files refer to host-selected regular files; they are separate from the Orb's filesystem mounts.
Devices, sockets, terminals and FIFOs are refused. Native file operations run inline in bounded
chunks; an individual operating-system file call is not preemptible by the callback deadline.
Native byte traffic stays in Rust after installation. Browser hosts can supply
`isolated.providers.stdio(orb, configuration)` returning `stdin.read(maxBytes, signal)`,
`stdout.write(bytes, signal)` and `stderr.write(bytes, signal)` providers, each with optional
`close()`. Each revision receives fresh providers. Reads and writes are serialized, bounded by
64 KiB and owned until their physical promises settle, even after cancellation. A provider's
promise must represent its actual work. Browser file selections require such an explicit adapter.
An injected provider is the default for its stream unless the installation explicitly selects
`"empty"` input or `"discard"` output.
Browser clock/random providers can likewise be injected through `isolated.providers.clocks`
and `isolated.providers.random`. Without an injected random provider, secure bytes use
`crypto.getRandomValues`; unavailable cryptography is an error, never a `Math.random` fallback.

The `"cli"` grant provides only the supported environment, arguments, standard I/O, terminal
queries and revision exit interfaces. It does not allow spawning native processes or grant
filesystem, network, clock or random access. Native process execution, when needed, must be
provided by an application-owned host adapter with separate, explicit authorization for the Orb.
Tembra does not currently provide a native process adapter.

`wasi:cli/exit` ends only the invoking revision, including inside a guest try/catch, and reports
its exit status. Supplied adapters are not terminals, so terminal getters return none. The
unstable timezone interface has no implementation in the pinned upstream release.

jco declares `stream<u8>` as `AsyncIterable<number>`. `readChunks` and `writeChunks` preserve
those declarations while transporting raw chunks of at most 64 KiB. Write pumps apply
backpressure and end with their originating callback. Directory streams yield typed entries.
Await the completion future as well as iterating the stream: a producer error can end iteration
and is reported by that future. A completed read whose reply is abandoned poisons its stream,
so a later callback receives an error instead of silently skipping bytes or directory entries.

```ts
import { readChunks, writeText } from "@syntropika/tembra-sdk";
import { getDirectories } from "wasi:filesystem/preopens@0.3.0";

// Standard calls run inside callbacks, after installation has supplied the granted preopen.
async function updateAndRead(consume: (chunk: Uint8Array) => void): Promise<void> {
  using root = getDirectories()[0]![0];
  using file = await root.openAt(
    {},
    "data.bin",
    { create: true, truncate: true },
    { read: true, write: true },
  );
  await writeText(file, "hello");
  const [stream, done] = file.readViaStream(0n);
  for await (const chunk of readChunks(stream)) consume(chunk);
  if ((await done).tag === "err") throw new Error("read failed");
}
```

Ordinary WIT exports become services named `<orb>::<package>/<interface>@<version>`. Acquire them with the
string form of `host.get`. Kebab-case WIT names stay kebab-case on the reference, for example
`reference["resource-value"]()`.

WIT `u64` and `s64` arguments and results use `bigint`, even for `0n` and `1n`.
Tembra preserves their full range across Orb calls, including nested records, lists, options,
variants and results. A JavaScript `number` or numeric `string` in a 64-bit position is refused;
use `bigint` before any arithmetic that could exceed the safe integer range. WIT `string` values
are never reinterpreted as integers.

The interpreter is loaded from `@syntropika/tembra-engine-node` when the first source orb is
installed. Hosts that install only compiled components never load it. `isolated.onLog` receives
console records attributed to the source Orb.

Both hosts accept these `isolated.limits` options:

| Option        | Default | Purpose                                         |
| ------------- | ------: | ----------------------------------------------- |
| `memoryBytes` | 128 MiB | Maximum Wasm linear memory per instance         |
| `packetBytes` |   8 MiB | Maximum packet size across the interpreter seam |
| `timeoutMs`   |  10,000 | Callback deadline in milliseconds               |

Node accepts `fuel` (default 100,000,000 Wasmtime units). Browser source Orbs use
`instructionTicks` (default 10,000 interpreter interrupt checks); prepared components use
`fuel` (default 100,000,000 instrumented core ticks). These are different execution budgets,
not comparable instruction counts. Native source Orbs also have the interpreter's fixed
10,000-check interrupt budget in addition to Wasmtime fuel.

Inside an isolated orb, import `orb` from `@syntropika/tembra-sdk` as usual. The build selects the
guest implementation. Guests can use state, selected dependencies, resources and granted I/O.
Source Orbs declare and contribute `series`, `waterfall` and `bail` hooks through their WIT
contracts. Compiled Orbs can contribute to those hooks. Tasks, listeners, state subscriptions
and compiled hook providers are not yet available. See [extensions](extensions.md).

Source methods can select a dependency instance for each request through `context.get`:

```ts
import type { Context } from "@syntropika/tembra-sdk";
import type { ExampleKeyedCounter100 as Counter } from "./contract.wit";

async function route(_environment: unknown, context: Context, sessionId: string) {
  const counter = await context.get<typeof Counter>("example:keyed/counter@1.0.0", [sessionId]);
  return counter.add(1);
}
```

The binding must be a WIT Orb import with an installed provider. Tembra applies its required
API version and methods, uses the admitted caller's principal, and waits for readiness.
References acquired this way belong to that root and are released when it ends; retaining a
JavaScript variable does not extend their authority. Calls from another root and acquisitions
from cleanup callbacks are refused. A declared dependency has its factory scope's lifetime
instead. Plain compiled WIT imports select the default `main` instance. A compiled Orb can select
keys through a [typed WIT reference view](tooling.md#keyed-dependency-references), with the same
root ownership and admission rules.

## Incoming HTTP

An Orb exports `wasi:http/handler@0.3.0` in its WIT world. The application keeps its listener,
authentication and routing, then invokes the installed Orb through `host.handleHttp`:

```ts
// `host` has installed the handler Orb. The application supplies the authenticated principal.
const response = await host.handleHttp("web", new Request("https://app.example.test/events"), {
  principal: "alice",
  signal: controller.signal,
});
// Forward this streaming Response through the application's server or browser integration.
```

The response head arrives before the body ends. Tembra owns a root for the exchange until its
physical work settles, so returning the response does not expire dependency-call authority.
Cancelling the signal cancels that root. Cancelling the response body signals disconnect;
an early return from a raw response body's `for await` loop does the same. The revision's
`timeoutMs` covers the whole exchange, including streaming and backpressure. A timeout
quarantines the revision; configure it for the expected SSE duration.

For lossless transport, pass an `IncomingHttpRequest`: `method`, paired `scheme`/`authority`,
`pathWithQuery`, ordered `fields: [string, Uint8Array][]`, an `AsyncIterable<Uint8Array>` body
(or `null`), and optional trailers. The returned `HttpHandlerResponse` has `status`, `fields`,
`body` and a `trailers` promise. Consume the body before awaiting trailers. This preserves raw
bytes, duplicate fields and trailers without collecting the complete body.

The WHATWG form returns a `Response`, joins duplicate headers according to that platform and
normalizes request URLs. It refuses 1xx responses and hidden fields; nonempty trailers fail
the body at its end because WHATWG `Response` cannot expose them. Use the raw form when these
properties matter. A handler's standard refusal is an `HttpHandlerError` with its typed
`wasi:http/types.error-code`; a trap or timeout remains an execution failure.

Source handlers are exercised in Chrome and Node; Node also exercises compiled handlers.
The SDK mobile carrier and the React Native bridge have separate qualification gates.
Browser components need an application-owned prepared adapter as described below. A handler-only
Orb needs no outgoing HTTP grant; an import of `wasi:http/client` still needs one.

## Browser and Worker

Import `createHost` from `@syntropika/tembra-sdk/browser` and await it.
It loads `@syntropika/tembra-engine-wasm` unless the application supplies a kernel or WASM module.
Serve the generated WASM asset with its loader, or set `wasmInput`.
The same entrypoint works in a module Worker.

Browser hosts install isolated source orbs through the same reusable dquickts Wasm module.
Pending host effects are awaited outside Wasm; JSPI is not required. Register filesystem providers with
`memoryFilesystem()` or `opfsFilesystem()`. Browsers have no ambient TCP implementation; an embedding may supply a transport provider.
Standard filesystem calls use the same providers: OPFS resolves direct handles,
reads only the requested range and commits streamed writes once per stream. It does not mirror
the directory into memory. Memory storage supports file and directory operations; OPFS supports
file operations and directory enumeration/create/remove, with `unsupported` for directory
rename where the browser has no implementation. Browser adapters do not emulate hard links,
symlinks or setting timestamps. Read-only grants are enforced before provider effects.

Compiled components are prepared during the application build with `browser: true` (see
[tooling](tooling.md#prepare-a-compiled-orb-for-browsers)). The application imports and registers the
resulting adapter explicitly:

```ts
import arithmetic from "./dist/tembra/arithmetic/browser/index.js";

const host = await createHost({ isolated: { components: [arithmetic] } });
await host.load("./dist/tembra/arithmetic/orb.<sha>.wasm");
```

JCO runs only during preparation. The browser receives generated bindings and instrumented core
modules; each installation gets separate memory, providers, grants and execution budgets.
The verifier matches the uploaded artifact's SHA-256 against the registered adapter. An arbitrary
compiled upload cannot introduce executable JavaScript. Prepared components require JSPI and support
typed data imports, exports, contributions and standard WASI adapters. Inter-Orb WIT resources,
streams and futures are refused in the browser; compiled hook definitions remain outside this
profile. Source Orbs remain dynamically loadable without JSPI or a per-Orb adapter.

Custom filesystem providers implement `FilesystemProvider.open()` and return an owned
`MountedFilesystem` with `openRoot()` and `dispose()`. Descriptors expose typed byte, metadata
and directory operations; `FilesystemError` carries a standard error code. The optional
`FileDescriptor.writeStream()` batches sequential chunks into one provider operation; providers
without it receive bounded `write()` or `append()` calls instead. Relative `host.load()`
locations resolve against `baseURL` or the page location.

The packaged interpreter is resolved automatically. Set `isolated.engine` only to override its
location (an absolute URL in browsers, bytes in Node). Browser hosts can supply `wasmInput` for the kernel
and `isolated.contractsInput` for the contract validator when the application owns asset loading;
ordinary installations use the packaged defaults.

Let the application bundler emit the engine package's Wasm assets, or serve its loaders beside
their Wasm files. If dependency prebundling moves a loader without preserving those asset paths,
exclude the engine package from that step or configure the explicit asset inputs above.

## React Native

Import `createHost` from `@syntropika/tembra-react-native` for iOS and Android.
It creates a host synchronously through the native TurboModule.
See the [React Native guide](react-native.md) for installation and cleanup.

## Packages

Node uses `@syntropika/tembra-engine-node` and its compiled native addon.
Build the engines for a local checkout; SDK source imports do not compile Rust.
Bun uses the SDK's TypeScript source exports. Node's default ESM exports use built JavaScript.
The root Git package points to SDK sources and declares exact engine package versions.
Git consumers resolve those engines separately; local development uses workspace links.
The root manifest defines the Bun workspaces. The lockfile and tool versions are also at the root.

See [SDK entrypoints](../packages/ts/sdk/package.json), [Node host](../packages/ts/sdk/src/node.ts)
and [browser host](../packages/ts/sdk/src/browser.ts).

## Owner-bound hook observation

A hook bound by `self.hook(name)` exposes `observe(source, payload)` as well as ordinary `run`.
Observation admits one selected hook plan for the original physical owner, including after caller
cancellation. The payload remains application-defined. Keep the primary result separate:

```ts
const primary = await execute(ctx);
let observerError;
try {
  await finished.observe(ctx, describe(primary));
} catch (error) {
  observerError = error;
}
reportObserverError(observerError);
return primary;
```

A successful admission is single-use across that source invocation's bound hooks; an empty plan or
observer failure still consumes it. Refused validation does not consume it. A bare root, settled
source, foreign owner or observer-dependent call cannot admit observation. An object-bound hook
requires its source object. An orb-bound hook preserves the actual source object's ownership.

JavaScript rejects a root-only source with `forbidden/observation_source`, described by
`Errors.Forbidden.ObservationSource`; Rust reports `invalid/context` for the same invalid source.
Both refuse before creating an observation or consuming admission.

The shared Rust kernel preserves the original principal, exact versioned plan and physical owner.
The observation context has an empty metadata frame; put correlation in the explicit payload.
Original cancelled contexts and expired frames remain unusable, and storage validates closed tokens.
Closing owners and version gates can refuse observation. Await it inside the still-owned physical
callback before returning; a logically cancelled caller may already have returned. The existing
runtime tracks physical work through cleanup. Releasing a completed temporary dependency does not
cancel observation. Cancellation remains cooperative and cannot undo or authorize replay of effects.

Calls made inside observation additionally require exact registered orb/object scopes and their
registered ancestors to exist and remain open. A missing scope returns `gone/scope`; a closing
scope or ancestor returns `conflict/scope_closed`. These checks apply to every dependent call;
ordinary calls retain their existing admission rules.
