import { Type, defineFacet, local, method } from "@tembra/sdk";
import type { ObservationBatch, ObservationOptions, TembraHost } from "@tembra/sdk";

import { check, defined } from "./assertions.ts";

type ObservedHost = Pick<TembraHost, "install" | "get" | "root" | "close" | "observations">;
export type ObservationHostFactory = (options?: {
  readonly observability?: ObservationOptions;
}) => Promise<ObservedHost>;

function collected(batch: ObservationBatch | null): ObservationBatch {
  check(batch !== null, "collection is enabled on this real runtime");
  return batch;
}

function healthy(batch: ObservationBatch): void {
  check(batch.statistics.dropped_events === 0, "the observation queue lost no events");
  check(batch.statistics.identifier_exhaustion === 0, "diagnostic identities remain admitted");
  check(batch.events.length <= batch.statistics.capacity, "queued metadata stays bounded");
}

/** Runs against an actual Node or browser Host, including its managed final drain. */
export async function observationAcceptance(createHost: ObservationHostFactory): Promise<void> {
  const disabled = await createHost();
  try {
    check(disabled.observations(4) === null, "collection defaults to disabled");
    await disabled.root((context) => {
      check(context.observationId === null, "disabled Context has no diagnostic operation");
      check(
        /^[0-9]+$/u.test(context.observationHostId),
        "diagnostic Host domain is a decimal string",
      );
    });
  } finally {
    await disabled.close();
  }
  check(disabled.observations(4) === null, "disabled managed shutdown has no collection");

  const contract = local("test.observations.boundary", {
    version: 1,
    input: Type.Object({}),
    members: {
      count: Type.Integer(),
      add: method(Type.Object({ amount: Type.Integer(), secret: Type.String() }), Type.Integer()),
    },
  });
  const host = await createHost({ observability: { capacity: 8192, sample_every: 1 } });
  let closed = 0;
  let provider: { host: string; id: string | null } | undefined;
  let rootIdentity: { host: string; id: string | null } | undefined;
  try {
    await host.install(
      defineFacet({
        name: "observed-provider",
        uses: [],
        setup(_scope, declaration) {
          declaration.serve(
            contract,
            (_input, self) => {
              const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
              self.resource("observed-resource", {
                open: () => ({}),
                close() {
                  closed += 1;
                },
              });
              return {
                state: { count },
                methods: {
                  add(context, value) {
                    provider = { host: context.observationHostId, id: context.observationId };
                    return count.replace(context, count.value + value.amount).value;
                  },
                },
              };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    const first = await host.get(contract, {});
    const second = await host.get(contract, {});
    let seen = 0;
    const stop = first.count.subscribe((value) => {
      seen = value;
    });
    try {
      await host.root(async (context) => {
        rootIdentity = { host: context.observationHostId, id: context.observationId };
        check(
          (await first.add({ amount: 3, secret: "excluded-payload-sentinel" }, context)) === 3,
          "instrumentation preserves the method result",
        );
      });
      check(seen === 3, "instrumentation preserves State delivery");
      check(host.observations(0)?.events.length === 0, "zero drain does not consume events");
      const active = collected(host.observations(8192));
      healthy(active);
      const claims = active.events.filter(
        (event) => event.operation === "claim" && event.outcome === null,
      );
      check(
        new Set(claims.map((event) => event.id)).size >= 2,
        "fresh acquisitions retain distinct claim identities",
      );
      check(
        active.events.some(
          (event) => event.operation === "physical_invocation" && event.root !== null,
        ),
        "real method dispatch exposes physical invocation and root metadata",
      );
      const identity = defined(provider);
      check(
        identity.id !== null &&
          active.events.some(
            (event) =>
              event.operation === "physical_invocation" &&
              event.host === identity.host &&
              event.id === identity.id,
          ),
        "the actual provider getter joins exactly to its Core physical invocation",
      );
      const root = defined(rootIdentity);
      check(
        active.events.some(
          (event) => event.operation === "root" && event.host === root.host && event.id === root.id,
        ),
        "a root Context getter describes the root rather than fabricating physical identity",
      );
      check(
        active.events.some((event) => event.operation === "state_delivery"),
        "real State deliveries are observed",
      );
      check(
        !JSON.stringify(active).includes("excluded-payload-sentinel"),
        "default metadata excludes argument payloads",
      );
    } finally {
      stop();
      first.release();
      second.release();
    }
  } finally {
    await host.close();
  }
  check(closed === 1, "managed resource closes once");
  const head = collected(host.observations(1));
  const tail = collected(host.observations(8192));
  healthy(tail);
  check(head.events.length <= 1, "final managed drain respects its limit");
  const final = [...head.events, ...tail.events];
  check(
    final.some((event) => event.operation === "resource_close"),
    "cleanup remains available after disposal",
  );
  check(collected(host.observations(8192)).events.length === 0, "final snapshot drains only once");
  check(defined(final[0]).id.length > 0, "final identities remain decimal strings");

  const overflowing = await createHost({ observability: { capacity: 1, sample_every: 1 } });
  try {
    check((await overflowing.root(() => 7)) === 7, "overflow cannot refuse application work");
  } finally {
    await overflowing.close();
  }
  const overflow = collected(overflowing.observations(8192));
  check(
    overflow.events.length <= 1 && overflow.statistics.dropped_events > 0,
    "overflow remains bounded and explicitly observable after cleanup",
  );

  const sampled = await createHost({ observability: { capacity: 512, sample_every: 2 } });
  try {
    await sampled.root(() => 1);
    await sampled.root(() => 2);
  } finally {
    await sampled.close();
  }
  const sample = collected(sampled.observations(512));
  healthy(sample);
  check(sample.statistics.sampled_out > 0, "sampling omissions remain explicit");
}
