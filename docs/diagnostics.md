# Errors and diagnostics

Rust and JS failures share three fields:

| Field     | Meaning                                              |
| --------- | ---------------------------------------------------- |
| `code`    | Error category, such as invalid input or a conflict. |
| `reason`  | A specific built-in or application-defined identity. |
| `message` | A short description for the reader.                  |

In TypeScript, use the exported descriptors:

```ts
import { Errors, TembraError } from "@tembra/sdk";

throw new TembraError(Errors.Invalid.StateValidation);
```

In Rust, use `Error`, `Code` and `Reason`.
Orbs can supply their own reason and message.
Check the code and reason in application logic; message text is for explanation.

## Inspect a host

`diagnostics()` reports counts for retained work and records.
The SDK also reports its pending work.
`errors()` drains failures collected outside the direct call result, such as observer failures.
`installedOrbs()` describes installed declarations without creating service objects.

## Observe calls

SDK hosts can enable `telemetry: { capacity, sampleEvery }`.
`host.traces()` returns buffered samples and clears them.
Each sample contains service, method, root, code version, duration and outcome.
The bounded buffer replaces its oldest sample when full.

Rust's optional `telemetry` module provides a bounded observation buffer.
Its caller records observations explicitly.
Exporters and permanent logs belong to application infrastructure.
Trace buffers hold metadata, not method arguments or state payloads.

See [Rust errors](../crates/core/src/error.rs), [Rust observations](../crates/core/src/telemetry.rs)
and [SDK traces](../packages/ts/sdk/src/telemetry.ts).
