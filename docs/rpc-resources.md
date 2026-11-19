# WIT resources over RPC

Generated TypeScript clients use the existing RPC connection to call native Rust Orbs and
compiled Wasm Orbs hosted by the Rust runtime. The browser or Tauri webview needs no prepared
component. Both provider types use the same portable resource delivery and connection claim table.

Generate the client from the provider's fixed WIT:

```ts
import { client } from "@syntropika/tembra-sdk/tooling";

await client({ wit: "protocol.wit", world: "provider", out: "src/clients" });
```

Use the generated client with an existing RPC connection:

```ts
import { RemoteHost } from "@syntropika/tembra-sdk/rpc";
import { get as getCodec } from "./clients/example-provider-codec.ts";

const connection = new RemoteHost(transport);
const codec = await getCodec(connection, { orb: "provider", key: "main" });
const decoder = await codec.beginDecoding(model);
const update = await decoder.push(frame);
decoder.release();
codec.reference.release();
```

The generated interface names and arguments follow the WIT. A resource constructor becomes
`codec.newDecoder(...)`; a static function such as `decoder.zero` becomes `codec.decoderZero()`.
Both synchronous and asynchronous WIT methods return promises to the RPC client. Synchronous
Rust implementations and their WIT signatures remain synchronous. An asynchronous `next()` method
suspends through the existing executor; it does not require a WIT stream or future.

Values keep the generated client's encoding: `bigint` for `u64`/`s64`, camelCase record fields,
`{ tag, val }` for variants and results, nullable options, and typed arrays for lists. Owned resources
can occur in the existing supported records, variants, results, lists, tuples and simple options.
Aliases retain the defining interface, version and resource name. Each issued handle also carries
its original provider revision.

## Ownership

A result owns a separate portable resource claim. Releasing the original service reference does
not release its returned resources. `resource.release()` is idempotent and ends that ownership.
Method receivers borrow the resource. The host processes release in frame order, even when
pending callbacks fill its admission budget. Releasing a resource prevents later frames from
reserving it while the admitted physical callback keeps its existing pin until it settles.

The SDK reserves a separate release budget of `maxPending` requests alongside the same-sized
ordinary request budget. Ordinary saturation does not prevent explicit release, failed-move
cleanup or disposal of late replies. Release responses free only release slots. Cleanup retains
no unbounded queue: if release responses themselves stall and exhaust that separate budget,
the SDK closes the connection with the exhaustion error, letting the host drain its claims.
A failed release write also closes with its original transport error because delivery is
uncertain. Calls are not replayed.

An `own<T>` argument consumes its wrapper. Validate and cancel a call before starting it when
ownership must remain with the caller. Once the SDK submits the move, failure, cancellation,
refused admission or an uncertain response never restores ownership. The host validates the full
batch before moving it; any undispatched moved batch closes on failure. Duplicate, foreign,
stale and cross-connection claims grant no authority. Moving an owner currently reserved by a
pending RPC method is refused before provider execution, consistently for both executors.

After a successful result, the SDK accepts its resource delivery. Cancellation before acceptance
closes every unaccepted owner, including results whose response was already queued. Late replies
are released rather than presented to a cancelled caller. Unaccepted owners share the existing
reference budget. A lost response without cancellation remains uncertain until cancellation or
connection closure; there is no separate cleanup timer. The embedding must report transport
loss and monitor the server connection's closure as described in [communication](communication.md).

Disconnect, explicit connection closure and host closure cancel pending calls and release the
connection's claims. Physical callbacks remain accounted until actual settlement. Cancellation
is cooperative and does not undo effects. A provider reload makes methods on old resources return
`gone/resource`; those old owners can still be released. The SDK never replays a call after
reconnecting. Acquire new clients and resources on the new connection.

Resource calls reuse the original reference's permissions and principal checks. A descriptor is
connection-local authority, checked before provider entry; it never bypasses kernel admission.
Unsupported capability delivery is refused before invoking a WIT function or constructor.
Generated local clients apply the same check, including ordinary data functions on a
resource-bearing interface. A missing or mismatched captured handler cannot execute an
ordinary provider callback. Plain data imports that explicitly accept a checked data result
retain that existing path; receivers and owned arguments always require resource delivery.

## Protocol

The resource protocol requires updated peers. Untyped data calls still use the common RPC
operations. Generated WIT clients, including data-only interfaces, use checked captured delivery
so `u64` and `s64` retain their entire ranges. Delivery is explicitly checked with `resources: 1` in `get`; the Rust host
returns the same value and the checked WIT service signature. All generated RPC clients require
that signature and capability support before any WIT method is called. The JavaScript RPC server
does not supply this checked WIT channel; generated RPC clients reject it during acquisition
before invoking provider code. Local data-only clients keep the ordinary local Reference path,
which already carries JavaScript `bigint` values directly. Local resource-bearing interfaces
continue to require matching captured delivery.

A captured call uses the existing `call` operation with `input` containing WIT JSON text and
`capabilities` containing its signature, optional receiver and owned arguments. Its response has
WIT JSON text in `output` and an `owned` descriptor array. Keeping WIT text inside the envelope
preserves wide integers and valid large integral floating-point values without changing the
ordinary RPC codec. Ordinary Rust RPC arguments and results retain numeric WIT integers;
generated clients use the existing captured text conversion rather than sending the generic
JSON codec's tagged BigInt objects into that numeric boundary. No global string conversion or
additional protocol is involved. Each descriptor contains a reference ID, a random token, defining interface,
resource name and provider revision. The existing `release` operation checks that descriptor.
The one-way `accept` operation acknowledges successful result delivery; `cancel` also discards
unaccepted results. No executor-specific resource protocol is used.

A call without resource delivery to a resource-bearing method is refused. Uncontracted
payload-only Rust handlers require checked WIT metadata for RPC; they are refused before callback
entry instead of discovering unsupported attachments after executing the callback. This is an
intentional behavior change. There is no legacy resource adapter or alternate executor protocol.
Ordinary serde methods use data calls.

## Restrictions

- Resource results must be issued by their defining interface. Foreign owned arguments retain
  the existing requirement to import that defining interface; the receiving executor must support
  the original claim. Native authored arguments redeem values only at their issuing object.
  Explicit `borrow<T>` parameters remain unsupported; method receivers keep their implicit borrow.
- New WIT stream/future profiles, maps, fixed-length lists and nested nullable options are excluded.
- Native macro exporters retain their existing restriction on authored resource structs nested
  inside records or variants. The portable transport supports those shapes; manual native portable
  exporters and compiled exporters exercise them in the RPC tests.
- Native hooks and the data-only Node/mobile and ordinary JavaScript provider adapters gain no new
  resource support from this change.
- Compiled providers retain the existing store scheduling, cooperative cancellation and trap
  quarantine rules. Logical cancellation never guarantees physical interruption.

## Verification

`just test-rpc-resources` builds a real Rust host, a macro-generated fixed-WIT decoder provider as
both native code and a component, and the existing resource-container component. The same generated
SDK clients run over actual bytes for both executor types. Data-only fixed-WIT clients verify
`u64::MAX`, `i64::MIN` and `i64::MAX` inside records, lists, tuples, nullable options, variants and
results, plus cancellation and refusal of an unprepared channel before provider execution.
Resource tests cover constructors, statics,
synchronous and asynchronous methods, moves, aliases, containers, cancellation, delayed replies,
ordinary saturation, bounded release cleanup, release while running, host closure, disconnect,
reload, forged and cross-session handles,
acquisition limits and oversized results. Rejected delivery is checked against provider counters.
Each host exits with no calls, roots or references remaining.

`just test-rpc-browser` runs the generated client in Chrome through a WebSocket bridge to that Rust
host for both provider types. `just test-native-rpc` includes the byte integration matrix, and the
browser acceptance suite includes the Chrome check. `just ci` runs repository checks; it does not
run the separate browser acceptance suite. These commands describe coverage, not a CI result.
