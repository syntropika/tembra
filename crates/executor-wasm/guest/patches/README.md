# Source Orb runtime patches

`runtime.patch` is one unified diff against the runtime crate and the stream helper of
[componentize-qjs](https://github.com/andreiltd/componentize-qjs) v0.6.0
(`758687536ea711ede17369f53a90ddc3e97a8dbb`). The runtime is built with
[dquickts](https://github.com/stencil-hq/dquickts) `b51e92c0` (it replaces rquickjs through a cargo
`patch.crates-io` override) and `wit-dylib-ffi` from wasm-tools v1.262.0.

Apply it from the upstream checkout root with `patch -p1 < runtime.patch`.

## Attribution

componentize-qjs is licensed under the Apache License, Version 2.0. The files this patch
changes stay under that license; the patch's modifications are Tembra's. Each modified file
is listed below with a summary, as section 4(b) of the license requires.

## What the patch changes

| File                                           | Change                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `crates/runtime/src/lib.rs`                    | 64 KiB QuickJS stack bound (dquickts stack checks must fit the linked Wasm shadow stack). Installs the promise hook and the quiescence state before any shim or user code. Wizer initialization fails when module evaluation leaves queued jobs or reachable pending promises.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `crates/runtime/src/call.rs`                   | One checked-scalar policy (`check_*`) for s8..u64, f32/f64 and char. It never coerces and never truncates modulo 2^N; 64-bit values are read through `JS_ToBigInt128Sat` without allocation. `pop_*` traps on invalid values, because export results have no error lane. `validate_import_arg` walks the real WIT parameter type (aliases, record, tuple, list, option, nested option, result, variant) and throws `TypeError`/`RangeError` before the host call. `stream`/`future` arguments are preflighted against exactly what lowering accepts (an idle endpoint of the matching type, an iterable or thenable inside an async call, or a raw `u32` handle), so an invalid one throws a `TypeError` before any earlier owned argument moves. A host `u32` above `i32::MAX` is lifted as a positive Number; upstream lifted it through `i32`, so it became negative. |
| `crates/runtime/src/endpoint.rs`               | `CopyEnd::check_transfer`: the non-mutating transfer check shared by the import preflight and `begin_transfer`.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| `crates/runtime/src/imports.rs`                | Calls the import validator. Refuses any WIT import while no export callback is entered: during module initialization (host authority closed) or during host cleanup.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `crates/runtime/src/bindings.rs`               | Private `__cqjs.failStream(reason)`, which traps with the reason's message.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
| `crates/core/src/js/stream-from.js`            | A failing producer calls `failStream` **before** the writable is dropped, so failure is never a clean EOF.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| `crates/runtime/src/quiescence.rs` (new)       | Callback entry state and promise quiescence. See below.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `crates/runtime/src/interpreter.rs`, `task.rs` | Every export entry gets an epoch. Async callbacks restore their task's epoch. Sync exports drain their jobs before returning. An async export that cannot settle traps. A boundary that leaves work behind traps. Resource destructors may not queue jobs.                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                               |

## Callback quiescence (why no job from call A can run as call B)

dquickts fires promise-hook `BEFORE`/`AFTER` only around thenable jobs, not around ordinary
reaction jobs (`quickjs.c` `promise_reaction_job`). A per-job epoch check would therefore need
an engine fork. The patch instead enforces this invariant: **every callback that completes an
export (sync return, async exit, module initialization) leaves an empty job queue and no
reachable pending promise.**

- Promises are tracked through the existing `INIT` hook with `WeakRef`s, which are not strong roots.
- The check collects cycles only when pending promises remain.
- `FinalizationRegistry` is removed before any code runs, because the collector queues its
  callbacks at arbitrary points.
- Under the host's existing one-call-at-a-time serialization, a continuation therefore only runs
  in the callback that registered or settled it.

Epochs are bookkeeping, not authority. No principal is stored in the guest.

Behaviour visible to authors (breaking):

- Work stored for a later call (an unresolved promise with a pending reaction, or a promise
  retained across calls) traps the call that left it, and the revision is quarantined.
- Detached work that holds a real host operation keeps its call open until that operation
  settles.

- `FinalizationRegistry` is unavailable.
- An async export that cannot settle through a pending host operation traps immediately.
- Concurrent export entry is refused while another callback is entered or suspended.
- An uncaught `queueMicrotask` exception traps its call. Rejected Promise reactions follow the
  export's result and quiescence rules.
- Invalid export values and stream/future payloads trap at canonical lowering. Recoverable
  argument validation covers the documented scalar shapes; other invalid shapes can still trap.

## Build inputs outside this patch

`dquickts.patch` applies to the pinned dquickts revision above. It guards `dtoa.c`'s
`setjmp.h` include on WASI and fixes `quickjs.c`'s nested `new` parsing. After parsing
`new X(args)`, the parser restores whether another call is allowed in the enclosing
expression. This preserves JavaScript precedence for emitted expressions such as
`new new Date(0).constructor()` and `new new Map().constructor()`.

The patched dquickts files retain their upstream MIT license. Both changes are local
Tembra modifications. The componentizer regression tests cover the emitted expressions,
reflective constructors and both frozen and granted clocks.

`guest/build.ts` also replaces the registry identities of exactly four rquickjs entries in
the pinned upstream lockfile with local paths. These edits do not change dependency
resolution. The build reconstructs and verifies both source trees before compiling with
`--locked`, records both patch digests and keys each source cache by its patch digest.

Public Orb platform imports use WASI 0.3. The runtime dylib currently targets `wasm32-wasip2`
and retains internal WASI 0.2 dependencies. Hosts resolve exact versioned import names and link
those internals through a separate fixed context with no preopens, network, arguments or
application environment. Internal clocks follow the clock grant; internal randomness is
predictable and must never be used as security material. Security-sensitive code must import
the granted public WASI random interfaces.
