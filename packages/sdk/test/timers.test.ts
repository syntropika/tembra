import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";

test("linger scheduling preserves unchanged deadlines and replaces earlier or later deadlines", async () => {
  let now = 0;
  const host = createHost({ now: () => now });
  await host.install(
    facet({
      name: "timers",
      apiVersion: 1,
      services: {
        slow: { linger: 100_000, methods: {} },
        fast: { linger: 10_000, methods: {} },
      },
    }),
  );
  let first = await host.get("slow", "first");
  const second = await host.get("slow", "second");
  let fast = await host.get("fast", "main");
  const scheduled = spyOn(globalThis, "setTimeout");
  const cleared = spyOn(globalThis, "clearTimeout");
  try {
    first.release();
    expect(scheduled.mock.calls.map((call) => call[1])).toEqual([100_000]);
    for (now = 1; now <= 10; now++) host.session.flush();
    expect(scheduled).toHaveBeenCalledTimes(1);
    now = 1000;
    second.release();
    expect(scheduled).toHaveBeenCalledTimes(1);
    now = 2000;
    first = await host.get("slow", "first");
    host.session.flush();
    expect(scheduled.mock.calls.at(-1)?.[1]).toBe(99_000);
    expect(scheduled).toHaveBeenCalledTimes(2);
    now = 3000;
    first.release();
    expect(scheduled).toHaveBeenCalledTimes(2);
    now = 4000;
    fast.release();
    expect(scheduled.mock.calls.at(-1)?.[1]).toBe(10_000);
    expect(scheduled).toHaveBeenCalledTimes(3);
    now = 5000;
    fast = await host.get("fast", "main");
    host.session.flush();
    expect(scheduled.mock.calls.at(-1)?.[1]).toBe(96_000);
    expect(scheduled).toHaveBeenCalledTimes(4);
    first = await host.get("slow", "first");
    const retainedSecond = await host.get("slow", "second");
    host.session.flush();
    expect(cleared).toHaveBeenCalledTimes(4);
    expect(scheduled).toHaveBeenCalledTimes(4);
    retainedSecond.release();
  } finally {
    first.release();
    fast.release();
    await host.close();
    scheduled.mockRestore();
    cleared.mockRestore();
  }
});
