import { facet } from "../src/facet.ts";
import { deferred } from "../src/runtime/records.ts";
import type { Host } from "../src/host.ts";
import type { Context } from "../src/runtime/context.ts";

/** Browser/Worker acceptance for root leases and exclusive-resource admission. */
export async function verifyExclusiveRootGate(host: Host): Promise<void> {
  const events: string[] = [];
  const entered = deferred<void>();
  const finish = deferred<void>();
  const releaseLate = deferred<void>();
  const context = deferred<Context>();
  const check = (condition: boolean, message: string) => {
    if (!condition) throw new Error(message);
  };
  const definition = (version: number) =>
    facet({
      name: "root-gate-acceptance",
      apiVersion: 1,
      services: {
        gate: {
          create(self) {
            return {
              connection: self.resource("connection", {
                version,
                exclusive: true,
                open() {
                  events.push(`open:${version}`);
                  return version;
                },
                close(value) {
                  events.push(`close:${value}`);
                },
              }),
            };
          },
          methods: { read: ({ connection }) => connection.value },
        },
      },
    });
  await host.install(definition(1));
  const reference = await host.get("gate", "main");
  const late = host.root(async (ctx) => {
    context.resolve(ctx);
    await releaseLate.promise;
  });
  const admitted = host.root(async (ctx) => {
    check((await reference.read(ctx)) === 1, "Initial resource missing");
    entered.resolve();
    await finish.promise;
    check((await reference.read(ctx)) === 1, "An admitted root lost its resource before ending");
  });
  let replacing: Promise<unknown> | undefined;
  try {
    await entered.promise;
    replacing = host.reload([definition(2)]);
    // With synchronous factories and no activities, the gate is reached before the next macrotask.
    await new Promise((resolve) => setTimeout(resolve, 0));
    check(events.join() === "open:1", "Exclusive resource closed while its root was active");
    finish.resolve();
    await admitted;
    await replacing;
    check(events.join() === "open:1,close:1,open:2", "Exclusive handoff ordering changed");
    const error = await reference.read(await context.promise).then(
      () => undefined,
      (error: unknown) => error,
    );
    check(
      (error as { reason?: string })?.reason === "upgrade_conflict",
      "An old root re-entered a closed resource",
    );
    check((await reference.read()) === 2, "Fresh root did not use the replacement");
  } finally {
    finish.resolve();
    releaseLate.resolve();
    await Promise.allSettled([admitted, late, replacing]);
    reference.release();
  }
}
