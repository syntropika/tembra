# Runtime observations and performance qualification

Tembra can record bounded runtime metadata without invoking synchronous user observers. Enable
collection at Host construction; acquire a consumer handle and drain it outside runtime callbacks:

```rust
use serde_json::json;
use tembra_core::{Host, HostOptions, ManualDriver, ObservationOptions};
let driver = ManualDriver::new();
let host = Host::new(
    HostOptions::new("application", json!(null))
        .with_observability(ObservationOptions::new(4096, 1)?),
    driver.clone(),
);
let observations = host.observations().expect("collection enabled");
// Execute application work, then export on an application-owned schedule.
let events = observations.drain(4096);
let statistics = observations.statistics();
let trace = tembra_core::observability::chrome_trace(&events);
# driver.run_until(host.close())?;
# Ok::<(), tembra_core::RuntimeError>(())
```

Node and browser Hosts accept `observability: { capacity, sample_every }`. Their
`host.observations(limit)` returns `{ events, statistics }`, or `null` when disabled. After managed shutdown, the SDK retains one bounded final plain-data snapshot so cleanup events
can still be drained without retaining native runtime handles. Rust and
JavaScript observe the same core; bindings do not create another invocation protocol. Decimal
strings preserve identifiers and nanosecond timestamps in serialized metadata. Draining consumes
events, so an application should choose one collector and share its exported results.

## Cost, bounds and failure behavior

The default path holds no collection, allocates no diagnostic IDs or labels, and reads no clock
for diagnostics. An enabled queue holds at most its checked capacity (1 through 1,048,576 events).
Each queued event has fixed scalar metadata, a label of at most 160 UTF-8 bytes, and at most a
160-byte failure reason. It never retains an object, Reference, Context, argument, result, callback
or native `Any`. Active span metadata is proportional to actual in-flight runtime owners; the
queue limit is not a cap on application concurrency or process memory. Collection handles and
spans retain the collection and only a weak Driver link. They do not prolong a scheduler. Queue health statistics have fixed cardinality.

When full, the queue drops new events and increments `dropped_events`; draining resumes normal
admission. Sampling selects every Nth operation, independently of whether its parent was sampled.
Start/end events from one operation share the same sampling decision. Parent IDs can refer to
sampled-out operations. Overflow can leave unmatched span pairs, so absence is not proof that an
operation never happened or is still running. `started`, `completed`, `abandoned` and `sampled_out`
count operations; `dropped_events` counts individual event records. Counters saturate. Diagnostic
identifier exhaustion increments its own counter and stops admitting new spans without refusing
runtime work. IDs are scoped to one Host collection; object identity is never used as claim identity.

Collection performs no filesystem/network output, payload serialization, or application observer
callback. Export adapters consume already-drained data. Allocator failure can still abort a process;
the existing trusted Driver supplies the clock and must not panic or call back into the Host from
`now`. Instrumentation does not attempt to catch arbitrary panics or make a trusted embedding
sandboxed. Existing handler, State observer, authorization and cleanup semantics remain in force.

## Timing and correlation

Timestamps use `Driver::now` relative to collection creation. When its weak Driver link no longer
upgrades, raw records retain an absent timestamp and `clock_unavailable` counts the missing values.
Chrome Trace omits such records and reports `omitted_missing_clock`; raw records and summary
missing-duration counts remain available. No zero or cached timestamp substitutes for a missing clock. Native and browser adapters supply
monotonic elapsed time; `ManualDriver` supplies logical time and cannot measure CPU consumption or
real latency. An async call duration includes readiness, suspension and nested work. Overlapping
spans must not be added as exclusive time. Host-owned executor poll spans measure the driver's
elapsed interval around a real `poll`; they are not thread CPU accounting. Other embedding tasks
and JavaScript engine jobs are outside those poll spans.

Operation IDs describe observed boundaries. Claims get independent IDs at actual acquisition;
Reference clones share the same claim. Root and code-version fields come from issued Contexts.
Nested invocation links use the parent operation carried by the actual Context. A call can begin
before a root is admitted; its end then supplies the root/version facts. No relation to an
application journal entry, durable task or model turn is inferred from object reuse or timing.
Remote Hosts have separate identity domains; RPC metadata does not invent a distributed trace ID.
Host IDs are local to a runtime thread. Exports spanning processes, threads or captures must add
their own collection namespace before joining `(host, id)` pairs.

A logical call result is distinct from final physical invocation lease release. `abandoned` means
the observed future/owner was dropped before recording a result. It does not establish that an
external effect was cancelled. `settled` records the final physical InvocationEntry release after
retained implementation owners are dropped. Resource and disposal spans describe their actual
managed operations, not universal external rollback. Root success records scope expiration, not
an application-level transaction commit. RPC send success records local enqueue acceptance;
it does not establish peer receipt, application completion or exactly-once effects.

`Context::observe` records a marker; it does not install its ID as the parent of subsequent calls.
It lets a trusted adapter record an explicit bounded boundary linked to an existing
issued Context. It confers no authority, checks no authorization, and does not extend the scope.
Adapters must still use the existing managed call, Context and invocation ownership APIs.

For an admitted method, Core installs its PhysicalInvocation diagnostic ID on the provider
Context before invoking the implementation. At that actual provider boundary,
`(context.host_id(), context.observation_id())` can therefore identify the exact physical entry.
A consumer must verify the matching `physical_invocation` record in the same collection and
healthy `identifier_exhaustion == 0` statistics. On diagnostic ID exhaustion, Context deliberately
falls back to its ancestor ID while application execution proceeds; an arbitrary Context ID
is not unconditionally a physical invocation. Sampling or overflow can remove a needed record.
Keep an unmatched sidecar unresolved rather than reconstructing physical identity from object,
root or timing. A Context marker does not change this relationship. Node/browser and embedded JavaScript expose
the same scalar diagnostic getters as `context.observationHostId` and `context.observationId`;
IDs remain decimal strings and an absent operation is `null`. Reading them delegates to the
existing issued Core Context without another owner or authority. An ended binding record can
refuse the read. Capture these scalars at the actual provider boundary, validate the matching
healthy event, and add the capture namespace before joining across threads or processes.

## Payloads

Metadata collection excludes principals, argument/result values, State values, error messages and
error details. Names and stable error reasons can themselves be sensitive; the embedding controls
collection and export access. There is no native `Any` serializer.

`capture_payload<T: Serialize>(&value, max_bytes)` is a separate explicit utility. It accepts an
already-known type, bounds emitted JSON to 1 through 65,536 bytes, and returns a typed error on
limit/encoding failure with no partial result. It does not run in core collection or automatically rewrite an
invocation result. Callers must handle diagnostic capture failures separately to preserve the
provider result. The chosen serializer can itself allocate, block, panic or have
effects; output-byte bounds do not isolate that implementation. Callers decide whether and where
to associate such a capture with an observed ID and how to redact it. No general payload capture
is implied for native calls, external resources or opaque binding values.

## Coverage boundaries

| Surface | Implemented boundary | Deliberate limit or remaining gap |
| --- | --- | --- |
| Object/reference | Host hold and Context.get admission with explicit created-claim links, unique claim lifetime, activation and disposal | Internal candidate/dependency hold variants do not all pass through public hold admission; claim construction still records their actual claim. Context.get links only the actual claim it creates, without reassigning Claim ownership. Object phases are not an independent event log. |
| Native invocation | Method attempt/result, native type refusal, actual Context/root and physical lease lifetime | Native-only pre-host/backing refusal can precede collection; no automatic value serialization. |
| Portable invocation | Readiness, cleaning/validation, result/error and remote call attempt | Validation stages are included in the call interval, not separately split per schema instruction. |
| Roots/Context | Local and imported roots, real parent operation carried into child invocation | Child scopes and observed cancellation-tree signals are recorded; detached tokens with no issued Context have no fabricated root. |
| State | Subscription owners, FIFO publications and individual observer deliveries | Graph normalization/preparation/adoption are within callers; no separate stage durations or State value capture. Initial delivery is included in subscription admission rather than an individual delivery span. |
| Registries | Real snapshot reads, FIFO table publication; calls use normal Reference dispatch | No alternate registry wrappers. Registration preparation and individual registry observer durations remain within activation/publication. |
| Resources/lifecycle | Activation, open including dependencies, close, disposal and managed reload coordinator | Reload resource opening/restoration is observed; refusal before reload coordinator scheduling remains outside that coordinator span. |
| Hooks | Actual driver-owned pipeline and managed contribution calls, including physical receipts | Admission/refusal before driver scheduling is part of the caller; no synthetic wrapper handlers. |
| RPC | Actual ingress decode/dispatch and outgoing encode/enqueue, portable call roots | Frame payloads, per-connection/call IDs and cross-host trace propagation are not yet exposed in metadata. |
| Executors | Actual polls of Host-owned spawned tasks on native/WASM Drivers | Node scheduler jobs and embedded engine queues outside Host spawning are not individually timed. |
| Rust facade | Core observations through `host.core().observations()` and core HostOptions | Typed payload capture is explicit; no second facade collection. |
| Node bindings | Constructor option, core drain operation and typed SDK method | Scalar Context diagnostic getters delegate to the same Core; use the matching source-built native addon. |
| Browser bindings | Checked constructor option, core JSON drain and typed SDK method | Scalar Context diagnostic getters delegate to the same Core; matching WASM and generated ESM are required. Worker Hosts have separate identity domains. |
| Embedded JavaScript | Core method/physical ownership coverage and Host task poll boundaries | Actual method jobs include queue/conversion settlement; prepare_for_host records real evaluation lifetime, while prepare(host_id, ...) has no collector. These are trusted engine boundaries, with no promise of sandbox isolation or CPU accounting. |
| Benchmarks | Native/portable dispatch, fresh/reused claims and descriptors, schema validation, State fanout, registry lookup, lifecycle, concurrent composed turns | Hook series, memory RPC and Rust typed-facade/descriptor cases are included. Node/browser workloads cover held/fresh References, reused/fresh descriptors, State16 fanout, registry reads and actually pending concurrent turns; embedded V8 Criterion cases cover held/fresh portable async methods. These cases require their real adapters; setup, drain/export and final cleanup health are outside measured batches. No claim that native benchmarks qualify browser or Node behavior. |

The table records implementation coverage, not a promise that every platform has been executed.
Qualification evidence belongs to the external run artifacts, identified by exact source hashes.

## Reproducible benchmarks

The microbenchmarks use [Criterion](https://docs.rs/criterion/0.8.2/criterion/) for warmup, repeated
samples, confidence intervals, noise reporting and baseline comparisons. The composed latency
harness uses [HdrHistogram](https://docs.rs/hdrhistogram/7.6.0/hdrhistogram/) for measured individual
turn p50/p95/p99; Criterion's batch estimates are never described as those individual percentiles.

The composed workload performs four real native calls, reactive State fanout to 16 subscribers
and a registry lookup per turn. A handler actually suspends, and the harness checks the maximum
simultaneously pending handlers against requested concurrency. It is a runtime workload without
model inference, durable task storage or network latency. Its closed-loop percentiles describe
admitted turns under that workload and are not open-loop production latency SLOs. Each sample
starts an independent Host; explicit warmup samples are retained and marked in JSON output.
CPU time and allocations are reported as missing unless a separate profiler measures them.

Run from the repository root, choosing an external output directory:

```sh
python3 testing/performance/run.py --output "$HOME/benchmark-results/tembra" --label baseline --unchanged-runtime -- --save-baseline baseline
python3 testing/performance/run.py --output "$HOME/benchmark-results/tembra" --label candidate -- --baseline baseline
python3 testing/performance/run.py --output "$HOME/benchmark-results/tembra" --label sampled --sample-every 16 -- --save-baseline sampled
python3 testing/performance/run.py --output "$HOME/benchmark-results/tembra" --label turns --bench composed
```

The runner records source hashes (including new files), dependency lock, compiler, build profile,
commands, selected executable hashes, exit status and child cleanup. It preserves Criterion's
analysis files and every composed sample outside the checkout. A failed or inconclusive run is
retained. Standard Criterion CLI parameters configure noise/significance and sample timing. The
runner does not turn a statistical difference into an automatic merge or a production threshold.
The runner uses the same `observability-bench` feature and binary for candidate-disabled and
candidate-enabled runs; an omitted sampling argument leaves collection disabled. Only the
constructor sampling setting changes between those candidate runs. To compare an unchanged runtime, apply only the same
benchmark harness and benchmark dependencies to an isolated baseline checkout, leave the feature
disabled (`--unchanged-runtime`), and verify that no runtime sources differ from the chosen revision. Never reuse a
binary or Cargo fingerprint under a different source identity.
Use named, immutable baselines and retain all sample/error output. Regression thresholds belong to
the caller's measured noise budget. Initial runs report trends; they do not establish production
SLOs or justify gating automatically. Compare unchanged-runtime versus candidate-disabled before
comparing disabled versus enabled, and repeat an A/A control to expose drift. A matched workload,
toolchain, dependency lock, build profile and source/executable identity are required for a claim
about instrumentation overhead. A fast unrelated microbenchmark cannot exonerate all runtime paths.

## Binding workloads

After building the SDK and matching native addon, the Node workload runs in actual Node:

```sh
TEMBRA_BENCH_PROFILE=debug node testing/typescript/src/benchmarks/node.ts > "$HOME/benchmark-results/node-off.json"
TEMBRA_BENCH_PROFILE=debug TEMBRA_OBSERVABILITY_SAMPLE_EVERY=1 node testing/typescript/src/benchmarks/node.ts > "$HOME/benchmark-results/node-on.json"
```

Set the profile to the one actually built. Keep the addon and emitted SDK unchanged between
off/on runs. `TEMBRA_BENCH_SAMPLES`, `TEMBRA_BENCH_WARMUP`, `TEMBRA_BENCH_OPERATIONS`,
`TEMBRA_BENCH_BATCH` and `TEMBRA_BENCH_CONCURRENCY` configure independent Hosts, fixed batches
and actual pending handlers. Operation count must be divisible by batch size. Concurrency is
bounded to 64 for this workload's admission budget. Warmup samples remain present and marked.

After building the matching browser WASM, the existing isolated browser runner can serve the
same workload in an actual browser:

```sh
TEMBRA_BENCH_PROFILE=debug TEMBRA_BROWSER_WORKLOAD=benchmark bun testing/typescript/scripts/test-browser.ts > "$HOME/benchmark-results/browser-off.json"
TEMBRA_BENCH_PROFILE=debug TEMBRA_BROWSER_WORKLOAD=benchmark TEMBRA_BROWSER_QUERY='sample_every=1' bun testing/typescript/scripts/test-browser.ts > "$HOME/benchmark-results/browser-on.json"
```

Query parameters `samples`, `warmup`, `operations`, `batch`, `concurrency` and `capacity` configure
the browser workload. This is explicit local measurement and requires the browser capability
used by the repository acceptance runner; WASM compilation alone cannot substitute for it.
The runner closes its browser session and server. Enclose the complete command in an external
process supervisor to impose the caller's deadline and retain failure/cleanup receipts.

The Node artifact records before/after identity of source files, dependency locks, emitted SDK
and the explicitly selected addon. Browser artifacts identify source, locks, served WASM and
bundled workload. The declared profile can be absent and is not inferred from a filename.
Both adapters preserve raw fixed-batch `performance.now` elapsed samples and throughput, with
CPU and allocations absent. A zero elapsed batch marks clock resolution limits and has no
throughput estimate. They do not present batch percentiles as individual-call latency. Result
checks and the normal async loop belong to this workload and remain identical in controls.

Draining, metadata hashing, export and final managed cleanup occur outside batch timing. Each
batch and final cleanup validates loss/identity health. An enabled run with drops fails rather
than publishing successful recording-overhead samples. The embedded V8 Criterion cases use
`python3 testing/performance/run.py --output "$HOME/benchmark-results/tembra" --label v8 --bench executor`;
they require the real embedded-executor feature and preserve the same postcleanup health rule.
Module evaluation ownership is acceptance-tested without lifting cumulative engine admission
limits to run an unbounded preparation benchmark.

The unchanged-runtime flag applies to the Rust common harness. Against an older Node/browser SDK
without the observation API, the binding workload needs an explicit common-harness route that
skips unavailable collection calls while leaving the baseline runtime unchanged. That route is
not supplied here; do not present native controls as an old-SDK binding baseline comparison.
Binding composed turns use default call roots, whereas the Rust composed fixture carries its
issued turn Context. These are separately named workloads, without an implied cross-binding cost
ratio. Recording overhead excludes draining/export and is not end-to-end application telemetry cost.
