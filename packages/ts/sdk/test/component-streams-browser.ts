/** Real prepared producer/consumer components exercising streamed inter-Orb results. */
import { createHost, type PreparedComponent } from "../src/browser.ts";
import type { Host } from "../src/host.ts";

const FRAMES = "tembra-test:streams/frames@1.0.0";
const DIGEST = "stream-consumer::tembra-test:streams/digest@1.0.0";
type Reference = { release(): void; [method: string]: any };
const MAX = (1n << 64n) - 1n;

function equal(actual: unknown, expected: unknown, label: string): void {
  if (actual !== expected)
    throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
}

function checksum(count: number, size: number, take = count * size): bigint {
  let value = 0n;
  let total = 0;
  for (let chunk = 0; chunk < count; chunk++)
    for (let byte = 0; byte < size && total++ < take; byte++)
      value = (value * 31n + BigInt((chunk + byte) % 256)) & MAX;
  return value;
}

async function artifact(name: string): Promise<Uint8Array> {
  const response = await fetch(`/${name}.wasm`);
  if (!response.ok) throw new Error(`Missing ${name}`);
  return new Uint8Array(await response.arrayBuffer());
}

async function using(
  base: object,
  components: PreparedComponent[],
  run: (host: Host, producer: Reference, consumer: Reference) => Promise<void>,
): Promise<void> {
  const host = await createHost({ isolated: { ...base, components } });
  let producer: Reference | undefined;
  let consumer: Reference | undefined;
  try {
    await host.install({ artifact: await artifact("stream-producer") });
    await host.install({
      artifact: await artifact("stream-consumer"),
      requires: { [FRAMES]: { orb: "stream-producer", apiVersion: 1 } },
    });
    producer = (await host.get(`stream-producer::${FRAMES}`, "main")) as Reference;
    consumer = (await host.get(DIGEST, "main")) as Reference;
    await run(host, producer, consumer);
  } finally {
    consumer?.release();
    producer?.release();
    await host.close();
    for (const [key, value] of Object.entries(host.diagnostics()))
      if (
        [
          "objects",
          "references",
          "calls",
          "roots",
          "resources",
          "tasks",
          "values",
          "pendingJobs",
        ].includes(key)
      )
        equal(value, 0, `${key} after close`);
  }
}

/** Bounded ordered reads, early reader drop, parallel heads and root-owned retained readers. */
export async function verifyBrowserByteStreams(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, producer, consumer) => {
    for (const [count, size] of [
      [0, 16],
      [3, 2048],
      [2, 70_000],
    ] as const) {
      const result = await consumer.sum(count, size, 0, MAX);
      equal(result[0], BigInt(count * size), "complete byte count");
      equal(result[1], checksum(count, size), "ordered checksum across chunk boundaries");
    }
    const partial = await consumer.sum(100, 4096, 0, 1n);
    equal(partial[0], 1n, "early drop consumes only requested byte");
    equal(partial[1], 0n, "first byte content");
    equal(await consumer["drop-then-ping"](100, 4096), 2, "reader drop unblocks a live callback");
    equal(await consumer.many(4, 3, 512), 6144n, "several heads before reading");
    await consumer.abandon(100, 4096);
    await consumer.retain(100, 4096);
    equal(await consumer.retained(), 0n, "normal root end closes a retained stream");
    equal(await producer.ping(8), 9, "producer remains healthy after drops");
    equal(host.errors().length, 0, "no hidden stream failures");
  });
}

/** A producer's late trap is reported and quarantines that revision, after published bytes. */
export async function verifyBrowserByteStreamTraps(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  for (const mode of [1, 2])
    await using(base, components, async (host, producer, consumer) => {
      const outcome = await consumer.sum(2, 64, mode, MAX).catch((error: unknown) => error);
      if (!(outcome instanceof Error)) {
        if (typeof outcome[0] !== "bigint" || outcome[0] < 0n || outcome[0] > 128n)
          throw new Error("Trapped producer returned an invalid byte count");
        // A trap may discard an unread tail, as on native hosts; every delivered byte must
        // still be an exact ordered prefix, and physical failure must reach the host.
        equal(outcome[1], checksum(2, 64, Number(outcome[0])), "ordered prefix before trap");
      }
      const refused = await producer.ping(1).catch((error: unknown) => error);
      if (!(refused instanceof Error)) throw new Error("Trapped producer revision still serves");
      equal(await consumer.ping(1), 2, "producer failure does not quarantine its consumer");
      if (!host.errors().length) throw new Error("Producer trap became an unreported clean EOF");
    });
}

/** Root cancellation closes a quiet writer, and the consumer remains usable afterwards. */
export async function verifyBrowserByteStreamCancellation(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, producer, consumer) => {
    await host.root(async (context) => {
      const pending = consumer.sum(1, 1, 3, MAX, context).catch((error: unknown) => error);
      // The published producer call is admitted independently of the consumer's outer call.
      const until = performance.now() + 10_000;
      while (host.diagnostics().calls < 2) {
        if (performance.now() >= until) throw new Error("Producer stream was never admitted");
        await new Promise((resolve) => setTimeout(resolve, 1));
      }
      context.cancel();
      const outcome = await pending;
      if (!(outcome instanceof Error)) throw new Error("Cancelled stream call succeeded");
    });
    equal(await consumer.ping(4), 5, "consumer remains usable after cancellation");
    equal(await producer.ping(4), 5, "cooperative quiet producer settles after cancellation");
  });
}
