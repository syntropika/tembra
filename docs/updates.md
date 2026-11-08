# Updates

A replacement prepares new declarations before it adopts them.
The engine coordinates active work, resources and state.
The SDK exposes this process for JavaScript orbs and `.wasm` artifacts.
Rebuilding an isolated orb produces a new artifact that can replace the running one.
Trusted native declarations can also be replaced through the host. Their new callback
implementations must already be available in the application; Wasm artifacts can be loaded as data.

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
Both accept orb definitions and artifacts as `{ artifact, requires, grants }`.
Rust hosts use `Host::reload` with native or loaded Wasm orbs.
Start replacement outside a root that uses an affected scope.
Otherwise, replacement can wait for the root that is waiting for it.
A preparation deadline aborts an unfinished replacement.

Increase the state version when the stored shape changes.
Provide `migrate(previous, previousVersion)` in the state's options.
Migration and validation finish before the new state is adopted.

## Resource reuse and recovery

Within one native heap, a compatible resource keeps its physical value.
After commit, the new declaration supplies its callbacks.
Increase its resource version when the value or cleanup contract changes.

An exclusive replacement closes the previous resource first.
If preparation fails, the engine attempts to restore the previous resource.
External effects are not a durable transaction.

A replaced isolated orb receives a new Wasm instance and its new grants. Its resources
are reacquired in the new heap even when their declared versions match; guest values
cannot cross isolated heaps.
Roots admitted before commit keep the previous instance until the kernel releases their version.

See [core replacement](../crates/core/src/program/reload.rs) and [SDK replacement](../packages/ts/sdk/src/runtime/reload.ts).
