import { Type, defineFacet, defineRegistry, local, method } from "@tembra/sdk";
import type { ObservationBatch, ObservationOptions, ReferenceOf, TembraHost } from "@tembra/sdk";

import { check, deferredVoid } from "../assertions.ts";

type Host = Pick<TembraHost, "install" | "get" | "registry" | "close" | "observations">;
export type HostFactory = (options?: {
  readonly observability?: ObservationOptions;
}) => Promise<Host>;
export type WorkloadOptions = {
  readonly samples: number;
  readonly warmup: number;
  readonly operations: number;
  readonly batch: number;
  readonly concurrency: number;
  readonly observability?: ObservationOptions;
};
const Commands = defineRegistry<{ title: string }>("benchmark.binding.commands");
function descriptor() {
  return local("benchmark.binding.counter", {
    version: 1,
    input: Type.Object({}),
    members: {
      count: Type.Integer(),
      echo: method(Type.Integer(), Type.Integer()),
      advance: method(undefined, Type.Integer()),
      wait: method(Type.Integer(), Type.Integer()),
    },
  });
}
const Counter = descriptor();

function drain(host: Host): ObservationBatch | null {
  const batch = host.observations(1_048_576);
  if (batch !== null) {
    check(batch.statistics.dropped_events === 0, "recording overhead requires zero queue loss");
    check(
      batch.statistics.identifier_exhaustion === 0,
      "recording overhead requires admitted identities",
    );
  }
  return batch;
}

/** Fixed batches produce raw elapsed samples, without a second statistics framework. */
export async function bindingWorkload(createHost: HostFactory, options: WorkloadOptions) {
  const { samples, warmup, operations, batch, concurrency } = options;
  for (const value of [samples, operations, batch, concurrency]) {
    check(
      Number.isInteger(value) && value > 0 && value <= 4096,
      "positive bounded workload parameter",
    );
  }
  check(Number.isInteger(warmup) && warmup >= 0 && warmup <= 100, "bounded warmup parameter");
  check(concurrency <= 64, "binding concurrency must fit the workload admission budget");
  check(operations % batch === 0, "operation count must be divisible by batch size");
  const results: unknown[] = [];
  for (let sample = 0; sample < samples + warmup; sample += 1) {
    // Independent Hosts expose warmup and sample drift rather than reusing hidden state.
    // oxlint-disable-next-line no-await-in-loop -- Each independent sample joins the preceding Host's cleanup.
    const host = await createHost(
      options.observability === undefined ? {} : { observability: options.observability },
    );
    let gate = deferredVoid();
    let entered = deferredVoid();
    let pending = 0;
    let maximum = 0;
    let observedDeliveries = 0;
    const stop: (() => void)[] = [];
    const sampleResults: unknown[] = [];
    let reference: ReferenceOf<typeof Counter> | undefined;
    try {
      // oxlint-disable-next-line no-await-in-loop -- Setup is outside every measured batch.
      await host.install(
        defineFacet({
          name: "binding-workload",
          uses: [],
          setup(_scope, declaration) {
            declaration.serve(
              Counter,
              (_input, self) => {
                const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
                self.registerMethod(Commands, "advance", { title: "Advance" }, "advance");
                return {
                  state: { count },
                  methods: {
                    echo: (_context, value) => value + 1,
                    advance: (context) => count.replace(context, count.value + 1).value,
                    async wait(_context, value) {
                      pending += 1;
                      maximum = Math.max(maximum, pending);
                      if (pending === concurrency) {
                        entered.resolve();
                      }
                      try {
                        await gate.promise;
                        return value + 1;
                      } finally {
                        pending -= 1;
                      }
                    },
                  },
                };
              },
              { lifetime: "host" },
            );
          },
        }),
      );
      // oxlint-disable-next-line no-await-in-loop -- One held binding per independent sample.
      const held = await host.get(Counter, {});
      reference = held;
      const observe = () => {
        observedDeliveries += 1;
      };
      for (let index = 0; index < 16; index += 1) {
        stop.push(held.count.subscribe(observe));
      }
      const cases = {
        held: async () => {
          check((await held.echo(1)) === 2, "held dispatch result");
        },
        fresh_reference: async () => {
          const fresh = await host.get(Counter, {});
          try {
            check((await fresh.echo(1)) === 2, "fresh dispatch result");
          } finally {
            fresh.release();
          }
        },
        fresh_descriptor: async () => {
          const fresh = await host.get(descriptor(), {});
          try {
            check((await fresh.echo(1)) === 2, "fresh descriptor result");
          } finally {
            fresh.release();
          }
        },
        state_fanout16: async () => {
          const before = observedDeliveries;
          await held.advance();
          check(
            observedDeliveries - before === 16,
            "all actual State observers received the publication",
          );
        },
        registry_snapshot: () => {
          check(
            host.registry(Commands).table().get("advance") !== undefined,
            "actual registry lookup",
          );
        },
      };
      for (const [name, operation] of Object.entries(cases)) {
        for (let offset = 0; offset < operations; offset += batch) {
          drain(host);
          const started = performance.now();
          for (let index = 0; index < batch; index += 1) {
            // oxlint-disable-next-line no-await-in-loop -- Sequential dispatch is the explicitly measured micro workload.
            await operation();
          }
          const elapsed = performance.now() - started;
          sampleResults.push({
            case: name,
            offset,
            operations: batch,
            elapsed_ms: elapsed,
            operations_per_second: elapsed > 0 ? (batch * 1000) / elapsed : null,
            clock_resolution_limited: elapsed === 0,
          });
          drain(host);
        }
      }
      for (let round = 0; round < operations / batch; round += 1) {
        gate = deferredVoid();
        entered = deferredVoid();
        maximum = 0;
        drain(host);
        const started = performance.now();
        const turns = Array.from({ length: concurrency }, async () => {
          const first = await held.wait(0);
          const second = await held.echo(first);
          await held.advance();
          check((await held.echo(second)) === 3, "composed four-call result");
          check(
            host.registry(Commands).table().get("advance") !== undefined,
            "composed registry lookup",
          );
        });
        const completed = Promise.all(turns);
        try {
          // oxlint-disable-next-line no-await-in-loop -- Actual pending handlers open this gate; a failed turn must also reject the cohort.
          await Promise.race([entered.promise, completed]);
        } finally {
          gate.resolve();
          // oxlint-disable-next-line no-await-in-loop -- All physical turns settle before observing this cohort.
          await completed;
        }
        const elapsed = performance.now() - started;
        check(
          maximum === concurrency && pending === 0,
          "the requested handlers truly overlapped and settled",
        );
        sampleResults.push({
          case: "composed_four_calls_state16_registry",
          round,
          turns: concurrency,
          method_calls: concurrency * 4,
          observed_max_pending: maximum,
          elapsed_ms: elapsed,
          turns_per_second: elapsed > 0 ? (concurrency * 1000) / elapsed : null,
          clock_resolution_limited: elapsed === 0,
        });
        drain(host);
      }
    } finally {
      gate.resolve();
      for (const remove of stop) {
        remove();
      }
      reference?.release();
      // oxlint-disable-next-line no-await-in-loop -- Cleanup and the final health check are outside measured intervals.
      await host.close();
    }
    const health = drain(host)?.statistics ?? null;
    results.push({
      sample,
      warmup: sample < warmup,
      batches: sampleResults,
      final_health: health,
      clock: "performance.now elapsed milliseconds",
      cpu_ns: null,
      allocated_bytes: null,
    });
  }
  return {
    workload: "binding_dispatch_state16_registry_pending_v1",
    options,
    samples: results,
    interpretation:
      "raw fixed-batch elapsed samples; no individual-call percentiles or CPU attribution",
  };
}
