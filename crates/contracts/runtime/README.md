# Shared source runtime

`dquickts.wasm` is the pinned interpreter core imported by prepared TypeScript Orbs. It contains
immutable code; each installation supplies its own memory, table, snapshot and host functions.
Independent compiled components do not use it.

The runtime and engine revisions are pinned in `../../executor-wasm/guest/build.ts`. Build that
script through `just build-runtime <wasi-sdk> <cache-directory>` after `just build-node`. It uses
`NodeContract.sourceRuntime` to derive the core through the real componentizer, compares it with
this file, and packages the authoring dylib only when they match. Source edits reuse that dylib.
The browser build copies this core into the engine package; native libraries include it once.

For an intentional engine update, inspect the generated `<cache-directory>/dquickts.wasm`, replace
this file and update its digest in `../src/shared_runtime.rs`. Update the type digest there if the
core's module type changes. Rebuild the bindings and repeat the packaging check. Rebuild source
Orbs with the matching authoring runtime; there is no fallback to a different core version.
