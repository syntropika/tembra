// Asynchronous acquisition of an address whose previous instance is still physically closing.
//
// Everything runs on a real Host and kernel. A service's resource close is held on a gate, so
// the previous instance stays "disposing" until the test opens it.
import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { orb } from "../src/orb.ts";

function gate() {
  let open!: () => void;
  const promise = new Promise<void>((resolve) => (open = resolve));
  return { promise, open };
}

/** Let every queued microtask and one macrotask turn run. */
async function turn() {
  await new Promise((resolve) => setTimeout(resolve, 0));
}

async function closing() {
  const host = createHost();
  const created: number[] = [];
  const entered = gate();
  const release = gate();
  await host.install(
    orb({
      name: "store",
      apiVersion: 1,
      services: {
        conn: {
          create(self) {
            created.push(created.length + 1);
            self.resource("link", {
              open: () => 1,
              close: async () => {
                entered.open();
                await release.promise;
              },
            });
            return {};
          },
          methods: { read: () => 7 },
        },
      },
    }),
  );
  // The previous instance: acquired, then released while its close is gated.
  const previous = await host.get("conn", "a");
  previous.release();
  await entered.promise;
  return { host, created, release };
}

test("an async get waits for the exact previous instance to close, then creates once", async () => {
  const { host, created, release } = await closing();
  try {
    let settled = false;
    const next = host.get("conn", "a").finally(() => (settled = true));
    await turn();
    // Not refused, not revived and not overlapped: nothing new runs while the close does.
    expect(settled).toBe(false);
    expect(created).toEqual([1]);
    release.open();
    const reference = await next;
    expect(await reference.read()).toBe(7);
    expect(created).toEqual([1, 2]);
    reference.release();
  } finally {
    release.open();
    await host.close();
  }
});

test("other addresses and refused requirements are never held behind a close", async () => {
  const { host, created, release } = await closing();
  try {
    // Another key and another principal are other instances: acquired at once.
    const other = await host.get("conn", "b");
    const alice = await host.get("conn", "a", { principal: "alice" });
    expect(created).toEqual([1, 2, 3]);
    other.release();
    alice.release();
    // A requirement the service can never meet is refused at once, never retried.
    await expect(host.get("conn", "a", { methods: ["missing"] })).rejects.toThrow();
    await expect(host.get("conn", "a", { apiVersion: 9 })).rejects.toThrow();
    // Synchronous acquisition stays fail-fast.
    expect(() => host.hold("conn", "a")).toThrow("still closing");
    expect(created).toEqual([1, 2, 3]);
  } finally {
    release.open();
    await host.close();
  }
});

test("a cancelled Context.get stops waiting without a claim, factory or revived instance", async () => {
  const { host, created, release } = await closing();
  try {
    const before = host.diagnostics().references;
    await host.root(async (context) => {
      const waiting = context.get("conn", "a");
      await turn();
      context.cancel();
      await expect(waiting).rejects.toThrow();
    });
    // The physical close was not cancelled; it completes when its gate opens.
    release.open();
    await turn();
    expect(created).toEqual([1]);
    expect(host.diagnostics().references).toBe(before);
    // The address is free again for a later acquisition.
    const reference = await host.get("conn", "a");
    expect(created).toEqual([1, 2]);
    reference.release();
  } finally {
    release.open();
    await host.close();
  }
});
