import { KernelOp } from "../src/protocol.ts";
import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import type { Reload, Snapshot } from "../src/kernel.ts";

const counter = () =>
  facet({
    name: "counter",
    apiVersion: 1,
    services: {
      counter: {
        create(self) {
          return { state: self.state({ count: 0 }) };
        },
        methods: {
          increment({ state }, ctx) {
            return state.update(ctx, (value: { count: number }) => ({ count: value.count + 1 }));
          },
        },
      },
    },
  });

test("a consumer selector failure detaches that selector without rejecting owner adoption", async () => {
  const host = createHost();
  await host.install(counter());
  const reference = await host.get("counter", "main");
  const selected = reference.state.select<number>((value) => {
    if (value.count) throw new Error("Consumer selector failed");
    return value.count;
  });
  const seen: number[] = [];
  reference.state.subscribe((value) => seen.push(value.count));
  try {
    await reference.increment();
    expect(reference.state.value.count).toBe(1);
    expect(selected.value).toBe(0);
    expect(seen).toEqual([0, 1]);
    expect(host.errors()).toMatchObject([{ message: "Consumer selector failed" }]);
    await reference.increment();
    expect(seen).toEqual([0, 1, 2]);
    expect(host.errors()).toEqual([]);
  } finally {
    reference.release();
    await host.close();
  }
});

test("foreign state publication adopts the kernel snapshot despite a failing consumer selector", async () => {
  let facetId = 0;
  const values = new Map<number, { count: number }>();
  const host = createHost({
    native: {
      hasFacet: (id) => id === facetId,
      activate: async (object) => {
        const value = host.session.kernel.command<number>(KernelOp.Allocate);
        values.set(value, { count: 0 });
        host.session.kernel.command(KernelOp.StateInit, { object, value, version: 1 });
      },
      invoke: async (invocation) => {
        const previous = host.session.kernel.command<Snapshot>(KernelOp.Snapshot, {
          object: invocation.object,
        });
        const value = host.session.kernel.command<number>(KernelOp.Allocate);
        values.set(value, { count: values.get(previous.value)!.count + 1 });
        host.session.kernel.command(KernelOp.StateAdopt, {
          call: invocation.id,
          object: invocation.object,
          revision: previous.revision,
          value,
        });
      },
      value: (id) => values.get(id),
      dispose: async () => {},
      effect: async () => {},
      dropValue: (id) => {
        values.delete(id);
      },
    },
  });
  const prepared = host.session.kernel.command<Reload>(KernelOp.Prepare, {
    facets: [
      { name: "native", apiVersion: 1, services: [{ name: "native", methods: ["increment"] }] },
    ],
  });
  facetId = prepared.facets[0]!.id;
  host.session.kernel.command(KernelOp.Commit, { ticket: prepared.id });
  const reference = await host.get("native", "main");
  reference.state.select((value) => {
    if (value.count) throw new Error("Foreign selector failed");
    return value.count;
  });
  const seen: number[] = [];
  reference.state.subscribe((value) => seen.push(value.count));
  try {
    await reference.increment();
    const object = host.session.handles.reference(reference).object;
    const snapshot = host.session.snapshot(object);
    expect(reference.state.value).toEqual(host.session.value(snapshot.value));
    expect(reference.state.value.count).toBe(1);
    expect(seen).toEqual([0, 1]);
    expect(host.errors()).toMatchObject([{ message: "Foreign selector failed" }]);
  } finally {
    reference.release();
    await host.close();
  }
});

test("reference release detaches retained state subscriptions and selections", async () => {
  const host = createHost();
  await host.install(counter());
  const keeper = await host.get("counter", "main");
  const object = host.session.handles.reference(keeper).object;
  const binding = host.session.objects.get(object)!.binding!;
  const original = binding.subscribe.bind(binding);
  let active = 0;
  let computations = 0;
  const selections: unknown[] = [];
  const subscriptions = spyOn(binding, "subscribe").mockImplementation((listener) => {
    active++;
    const detach = original(listener);
    return () => {
      active--;
      detach();
    };
  });
  try {
    for (let index = 0; index < 1000; index++) {
      const reference = await host.get("counter", "main");
      reference.state.subscribe(() => {});
      selections.push(
        reference.state.select((value) => {
          computations++;
          return value.count;
        }),
      );
      if (index % 2) {
        const id = host.session.handles.reference(reference).id;
        host.session.kernel.command(KernelOp.Release, { reference: id });
        host.session.kernel.command(KernelOp.Forget, { reference: id });
        host.session.handles.revoke(id);
      } else reference.release();
    }
    expect(active).toBe(0);
    expect(computations).toBe(1000);
    await keeper.increment();
    expect(computations).toBe(1000);
    expect(selections).toHaveLength(1000);
  } finally {
    subscriptions.mockRestore();
    keeper.release();
    await host.close();
  }
});
