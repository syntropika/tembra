# Data and remote calls

Tembra separates engine metadata from application data.

| Boundary                           | Data path                                                     |
| ---------------------------------- | ------------------------------------------------------------- |
| Rust to Rust                       | Typed local values. No codec conversion for the call.         |
| JS to JS in one runtime            | Local values and callbacks. No codec conversion for the call. |
| JS to Rust in the embedded host    | The host codec encodes arguments and results.                 |
| Swift or Kotlin to the Rust kernel | JSON metadata commands and result envelopes.                  |
| React Native JS to the Rust kernel | A TurboModule sends metadata through Swift or Kotlin.         |
| Host to host                       | RPC sends encoded frames through an application transport.    |

The internal bridge uses JSON commands for declarations, identities and control.
It also carries scalar context metadata.
Regular state values and method arguments stay outside this metadata protocol.

## Native bindings

`TembraKernel.request()` exchanges JSON strings with the Rust metadata bridge.
UniFFI generates the bindings. Swift uses the C interface; Kotlin uses JNA on the JVM and Android.
Each request waits for the kernel result.
Kernel errors stay in the JSON result envelope. Binding failures throw native errors or exceptions.

React Native sends these requests through its TurboModule and the shared native bindings.
Orb callbacks, state values and method arguments stay in the JavaScript runtime.
The native packages expose metadata commands. They do not provide Swift or Kotlin orb execution or RPC clients.

See [Swift](swift.md), [Kotlin](kotlin.md) and [React Native](react-native.md) for platform use.

## Codecs

A **codec** converts portable data to bytes and back.
The host selects it. Both sides of a boundary must use the same contract.

The default JSON codec uses explicit tags for BigInt and bytes.
Use JS `BigInt` for integers outside the safe `Number` range.
Rust provides `codec::Integer` for portable decimal integers.
The optional `@syntropika/tembra-sdk/msgpack` codec preserves BigInts with an extension type.
Binary encoding alone does not fix numeric precision.
Provide a custom codec for other data types.

## Remote references

`serve(host, transport, options)` exposes a host.
`new RemoteHost(transport, options)` connects a client.
Import both from `@syntropika/tembra-sdk/rpc`.
The transport supplies `send`, `listen` and `close` over byte frames.
Server infrastructure supplies the trusted principal and acquisition policy.

A connection owns its remote references and state mirrors.
Disconnect invalidates them. Reconnect with a new peer and acquire current state again.
A disconnected call can have an unknown outcome; it is not retried automatically.
RPC does not copy a local context or opaque storage reference to another host.

## Native Rust server

Enable `tembra`'s `rpc` feature and use `tembra::rpc::serve(host, options, send)`.
Construct `Options::new(principal, allow)` from trusted infrastructure. The policy receives `None`
for catalog discovery and `Some(key)` for acquisition. Configure `max_frame`, `max_pending` and
`max_references` before serving. Enable `views` when using native service access policies.

Feed complete incoming frames to `Connection::receive`. The sender accepts an entire encoded frame
or returns an error; it must never block the host executor and must bound queued and in-flight bytes.
Perform blocking I/O elsewhere and refuse a full queue. Configure the client frame limit no higher
than the server limit: the SDK defaults to 1 MiB, while the native server defaults to 64 KiB.
Monitor `Connection::closed()` in the same transport pump as input/output and propagate
`close_cause()` when closing the real channel.
Use `close_with(error)` on transport failure. One task may await closure at a time. The connection
releases observations and claims and signals cancellation; managed callbacks can settle later.

Oversized initial State and results return `exhausted/frame_size` when the error fits. Oversized live
State and send failures close the entire channel. On a later connection, consumers can refuse to
reacquire that oversized view and acquire unrelated services. The embedding must map a terminal
resource cause to the transport's agreed close/error notification; JSON RPC adds no close envelope.
A disconnected call or oversized result can
have an unknown or already performed effect; Tembra never replays it.

SDK transports implement optional `close(cause?: unknown)`. `Channel.close` passes its terminal cause
after detaching listeners, so the embedding must report ownership closure in that method even when
its socket callback is detached. Handle or ignore the cause and wrap native platform close methods
rather than passing the cause as a socket close code. Nonfatal observer reports through `onError`
do not imply closure.

Run `just test-native-rpc` for actual Rust/SDK byte conformance.

## Optional infrastructure

`HostManager` starts, stops or replaces named hosts. Replacement creates new host memory.
`router()` selects a host before acquisition.
`coordinate()` prepares direct participants and decides commit or abort in memory.
It does not persist that decision or provide a durable distributed transaction.

See [Rust codec](../crates/tembra/src/codec.rs), [SDK codecs](../packages/ts/sdk/src/codecs/json.ts)
and [RPC transport](../packages/ts/sdk/src/infrastructure/rpc/channel.ts).
