# Updates

A replacement prepares new declarations before it adopts them.
The engine coordinates active work, resources and state.
The SDK exposes this process for JavaScript orbs.
Compiled native orbs stay fixed for the lifetime of their native host.

## Versions

| Version          | Meaning                                                 |
| ---------------- | ------------------------------------------------------- |
| API version      | The public orb contract that dependencies require.      |
| Code version     | The host's installed implementation snapshot for calls. |
| State version    | The shape of the object's stored state.                 |
| Resource version | The contract for a physical resource and its cleanup.   |

Compatible replacement keeps existing roots on their admitted code version.
New roots use the adopted version.
When a change needs exclusive access, the engine gates the affected scope.
It waits for roots that already used that scope, including their nested calls, and for owned tasks.
Older contexts cannot enter an upgraded scope after commit.

## Prepare, commit or abort

In the SDK, `host.reload([orb])` prepares and commits.
`host.prepare([orb])` returns a handle with `commit()` and `abort()`.
Start replacement outside a root that uses an affected scope.
Otherwise, replacement can wait for the root that is waiting for it.
A preparation deadline aborts an unfinished replacement.

Increase the state version when the stored shape changes.
Provide `migrate(previous, previousVersion)` in the state's options.
Migration and validation finish before the new state is adopted.

## Resource reuse and recovery

A compatible resource keeps its physical value.
After commit, the new declaration supplies its callbacks.
Increase its resource version when the value or cleanup contract changes.

An exclusive replacement closes the previous resource first.
If preparation fails, the engine attempts to restore the previous resource.
External effects are not a durable transaction.

See [core replacement](../crates/core/src/program/reload.rs) and [SDK replacement](../packages/ts/sdk/src/runtime/reload.ts).
