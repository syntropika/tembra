import { expect, test } from "bun:test";
import { disposeOrb, prepareDefinitions, type AdaptedOrb } from "../src/runtime/adapters.ts";
import { deferred } from "../src/runtime/records.ts";

const artifacts = (count: number) =>
  Array.from({ length: count }, (_, index) => ({
    artifact: new Uint8Array([index]),
  }));

test("preparation overlaps bounded native opens and preserves installation order", async () => {
  const releases = Array.from({ length: 6 }, () => deferred<void>());
  const fifthStarted = deferred<void>();
  const started: number[] = [];
  const preparation = prepareDefinitions(artifacts(6), async ({ artifact }) => {
    const index = artifact[0]!;
    started.push(index);
    if (index === 4) fifthStarted.resolve();
    await releases[index]!.promise;
    return { name: `orb-${index}`, apiVersion: 1 };
  });
  expect(started).toEqual([0, 1, 2, 3]);
  releases[3]!.resolve();
  await fifthStarted.promise;
  expect(started).toEqual([0, 1, 2, 3, 4]);
  releases.forEach((release) => release.resolve());
  expect((await preparation).map((orb) => orb.name)).toEqual([
    "orb-0",
    "orb-1",
    "orb-2",
    "orb-3",
    "orb-4",
    "orb-5",
  ]);
});

test("failed preparation drains concurrent opens before disposing every completed candidate", async () => {
  const releases = Array.from({ length: 6 }, () => deferred<void>());
  const started: number[] = [],
    disposed: number[] = [];
  const error = new Error("open failed");
  const preparation = prepareDefinitions(artifacts(6), async ({ artifact }) => {
    const index = artifact[0]!;
    started.push(index);
    await releases[index]!.promise;
    if (index === 1) throw error;
    return {
      name: `orb-${index}`,
      apiVersion: 1,
      [disposeOrb]: () => {
        disposed.push(index);
      },
    } as AdaptedOrb;
  });
  let settled = false;
  const outcome = Promise.resolve(preparation).then(
    () => {
      throw new Error("preparation unexpectedly succeeded");
    },
    (failure: unknown) => {
      settled = true;
      return failure;
    },
  );
  releases[1]!.resolve();
  await new Promise((resolve) => setTimeout(resolve, 0));
  expect(settled).toBe(false);
  expect(disposed).toEqual([]);
  releases.forEach((release) => release.resolve());
  expect(await outcome).toBe(error);
  expect(started).toEqual([0, 1, 2, 3]);
  expect(disposed).toEqual([0, 2, 3]);
});

test("preparation reports both the opening failure and concurrent candidate disposal failure", async () => {
  const opening = new Error("open failed"),
    disposal = new Error("dispose failed");
  const preparation = prepareDefinitions(artifacts(2), async ({ artifact }) => {
    if (artifact[0] === 0) throw opening;
    return {
      name: "opened",
      apiVersion: 1,
      [disposeOrb]: () => {
        throw disposal;
      },
    } as AdaptedOrb;
  });
  const error = await Promise.resolve(preparation).catch((error: unknown) => error);
  expect(error).toBeInstanceOf(AggregateError);
  expect((error as AggregateError).errors[0]).toBe(opening);
  expect((error as AggregateError).errors[1].errors).toContain(disposal);
});

test("concurrent opening errors retain definition order regardless of completion order", async () => {
  const releases = [deferred<void>(), deferred<void>()];
  const errors = [new Error("first"), new Error("second")];
  const preparation = prepareDefinitions(artifacts(2), async ({ artifact }) => {
    const index = artifact[0]!;
    await releases[index]!.promise;
    throw errors[index];
  });
  const outcome = Promise.resolve(preparation).catch((error: unknown) => error);
  releases[1]!.resolve();
  releases[0]!.resolve();
  expect(((await outcome) as AggregateError).errors).toEqual(errors);
});

test("a synchronous backend failure with an undefined reason still rejects preparation", async () => {
  const preparation = prepareDefinitions(artifacts(1), () => {
    throw undefined;
  });
  const outcome = await Promise.resolve(preparation).then(
    () => ({ failed: false, reason: undefined }),
    (reason: unknown) => ({ failed: true, reason }),
  );
  expect(outcome).toEqual({ failed: true, reason: undefined });
});

test("preparation rejects a backend that returns no definition", async () => {
  const preparation = prepareDefinitions(
    artifacts(1),
    async () => undefined as unknown as AdaptedOrb,
  );
  await expect(preparation).rejects.toThrow("Execution backend returned no Orb definition");
});

test("plain definitions retain their position around asynchronously prepared artifacts", async () => {
  const first = { name: "first", apiVersion: 1 },
    last = { name: "last", apiVersion: 1 };
  const prepared = await prepareDefinitions([first, ...artifacts(1), last], async () => ({
    name: "middle",
    apiVersion: 1,
  }));
  expect(prepared[0]).toBe(first);
  expect(prepared[1]?.name).toBe("middle");
  expect(prepared[2]).toBe(last);
});
