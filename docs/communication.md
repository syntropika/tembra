# Data and remote calls

Tembra separates engine metadata from application data.

| Boundary                        | Data path                                                     |
| ------------------------------- | ------------------------------------------------------------- |
| Rust to Rust                    | Typed local values. No codec conversion for the call.         |
| JS to JS in one runtime         | Local values and callbacks. No codec conversion for the call. |
| JS to Rust in the embedded host | The host codec encodes arguments and results.                 |
| Host to host                    | RPC sends encoded frames through an application transport.    |

The internal bridge uses JSON commands for declarations, identities and control.
It also carries scalar context metadata.
Regular state values and method arguments stay outside this metadata protocol.

## Codecs

A **codec** converts portable data to bytes and back.
The host selects it. Both sides of a boundary must use the same contract.

The default JSON codec uses explicit tags for BigInt and bytes.
Use JS `BigInt` for integers outside the safe `Number` range.
Rust provides `codec::Integer` for portable decimal integers.
The optional `@tembra/sdk/msgpack` codec preserves BigInts with an extension type.
Binary encoding alone does not fix numeric precision.
Provide a custom codec for other data types.

## Remote references

`serve(host, transport, options)` exposes a host.
`new RemoteHost(transport, options)` connects a client.
Import both from `@tembra/sdk/rpc`.
The transport supplies `send`, `listen` and `close` over byte frames.
Server infrastructure supplies the trusted principal and acquisition policy.

A connection owns its remote references and state mirrors.
Disconnect invalidates them. Reconnect with a new peer and acquire current state again.
A disconnected call can have an unknown outcome; it is not retried automatically.
RPC does not copy a local context or opaque storage reference to another host.

## Optional infrastructure

`HostManager` starts, stops or replaces named hosts. Replacement creates new host memory.
`router()` selects a host before acquisition.
`coordinate()` prepares direct participants and decides commit or abort in memory.
It does not persist that decision or provide a durable distributed transaction.

See [Rust codec](../crates/tembra/src/codec.rs), [SDK codecs](../packages/ts/sdk/src/codecs/json.ts)
and [RPC transport](../packages/ts/sdk/src/infrastructure/rpc/channel.ts).
