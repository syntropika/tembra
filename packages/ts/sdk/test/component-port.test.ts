import { expect, test } from "bun:test";
import { ComponentPort, type Callback } from "../src/interpreter/component/port.ts";
import type { CallbackScope } from "../src/interpreter/component/scope.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((yes) => {
    resolve = yes;
  });
  return { promise, resolve };
}

function harness(timeout = 1000) {
  let current: number | null = null;
  let disposed = 0;
  let nextId = 0;
  const port = new ComponentPort(
    timeout,
    async () => {
      disposed++;
    },
    100n,
  );
  port.exports = {};
  port.observer.attach({ currentRootTask: () => current });
  port.limits = {
    fuel: new WebAssembly.Global({ value: "i64", mutable: true }, 0n),
    pages: new WebAssembly.Global({ value: "i64", mutable: true }, 100n),
  };
  const start = (head: unknown = "head") => {
    const id = ++nextId;
    let cancelled = 0;
    let callback!: Callback;
    const scope = {
      check() {},
      cancel() {
        cancelled++;
      },
    } as unknown as CallbackScope;
    const task = port.start(BigInt(id), scope, () => {
      port.observer.taskStarted({ id, entry: "stream", isAsync: true });
      callback = port.current();
      return head;
    });
    return { ...task, id, callback, cancelled: () => cancelled };
  };
  const exit = (id: number) => port.observer.taskExited({ id });
  return {
    port,
    start,
    exit,
    current: (id: number | null) => {
      current = id;
    },
    disposed: () => disposed,
  };
}

test("parked callbacks retain distinct authority and physical ownership across entry handoff", async () => {
  const { port, start, exit, current, disposed } = harness();
  const head = deferred<string>();
  const first = start(head.promise);
  expect(() => first.park()).toThrow("successful component result");
  expect(() => start()).toThrow("busy");
  head.resolve("first");
  expect(await first.result).toBe("first");
  first.park();
  expect(() => port.current()).toThrow("No active host scope");

  const second = start("second");
  expect(await second.result).toBe("second");
  current(first.id);
  expect(port.current()).toBe(first.callback);
  current(second.id);
  expect(port.current()).toBe(second.callback);
  current(null);
  exit(first.id);
  await first.finished;
  expect(first.cancelled()).toBe(1);
  expect(second.cancelled()).toBe(0);
  expect(port.current()).toBe(second.callback);
  // Repeated parking of the earlier callback cannot revoke the current owner.
  first.park();
  expect(port.current()).toBe(second.callback);
  second.park();
  await expect(port.close()).rejects.toThrow("physical work");

  const physical = deferred<void>();
  port.physical(second.callback, physical.promise);
  exit(second.id);
  let settled = false;
  const drained = port.settled().then(() => {
    settled = true;
  });
  await Promise.resolve();
  expect(settled).toBe(false);
  await expect(port.close()).rejects.toThrow("physical work");
  physical.resolve();
  await second.finished;
  await drained;
  expect(second.cancelled()).toBe(1);
  await port.close();
  expect(disposed()).toBe(1);
});

test("overlapping callbacks and destructors never replenish their active group's fuel", async () => {
  const { port, start, exit, current } = harness();
  const first = start();
  await first.result;
  expect(port.limits!.fuel.value).toBe(100n);
  port.limits!.fuel.value = 70n;
  first.park();
  const second = start();
  await second.result;
  expect(port.limits!.fuel.value).toBe(70n);
  port.limits!.fuel.value = 40n;
  second.park();

  const destroying = port.destroy(() => {
    port.observer.taskStarted({ id: 99, entry: "<resource-drop>", isAsync: false });
    current(99);
    expect(() => port.current()).toThrow("destructor holds no call authority");
    current(first.id);
    expect(port.current()).toBe(first.callback);
    current(null);
    expect(port.limits!.fuel.value).toBe(40n);
  });
  expect(() => start()).toThrow("busy");
  // A parked producer can resume while an observed destructor awaits its exit notification.
  current(second.id);
  expect(port.current()).toBe(second.callback);
  current(null);
  port.observer.taskExited({ id: 99 });
  await destroying;
  expect(port.limits!.fuel.value).toBe(40n);
  exit(first.id);
  await first.finished;
  const third = start();
  await third.result;
  expect(port.limits!.fuel.value).toBe(40n);
  third.park();
  exit(second.id);
  exit(third.id);
  await Promise.all([second.physical, third.physical]);

  const fresh = start();
  await fresh.result;
  expect(port.limits!.fuel.value).toBe(100n);
  exit(fresh.id);
  await fresh.physical;
  await port.close();
});

test("parking preserves each callback deadline and one callback ending cannot clear another's", async () => {
  const { port, start, exit } = harness(80);
  const first = start();
  await first.result;
  first.park();
  await new Promise((resolve) => setTimeout(resolve, 25));
  const second = start();
  await second.result;
  second.park();
  exit(first.id);
  await first.finished;
  await expect(second.finished).rejects.toThrow("wall time limit");
  expect(second.cancelled()).toBe(1);
  expect(port.usable).toBe(false);
  await expect(port.close()).rejects.toThrow("physical work");
  exit(second.id);
  await expect(second.physical).rejects.toThrow("wall time limit");
  await port.close();
});

test("quarantine revokes bounded waits but retains actual task and provider work until proven exit", async () => {
  const { port, start, exit, current } = harness(20);
  const task = start();
  await task.result;
  task.park();
  const provider = deferred<void>();
  port.physical(task.callback, provider.promise);
  await expect(task.finished).rejects.toThrow("wall time limit");
  let released = false;
  void task.physical.then(
    () => {
      released = true;
    },
    () => {
      released = true;
    },
  );
  expect(task.cancelled()).toBe(1);
  expect(released).toBe(false);
  current(task.id);
  expect(port.current()).toBe(task.callback);
  current(null);
  await expect(port.close()).rejects.toThrow("physical work");
  exit(task.id);
  await Promise.resolve();
  expect(released).toBe(false);
  await expect(port.close()).rejects.toThrow("physical work");
  provider.resolve();
  await expect(task.physical).rejects.toThrow("wall time limit");
  expect(released).toBe(true);
  await port.settled();
  await port.close();
});

test("a destructor timeout quarantines promptly but retains cleanup until actual exit and provider close", async () => {
  const { port, current } = harness(20);
  const provider = deferred<void>();
  let completed = false;
  const destruction = port.destroy(() => {
    port.observer.taskStarted({ id: 99, entry: "<resource-drop>", isAsync: false });
    current(99);
    port.wasi.cleanup(() => provider.promise);
    current(null);
  });
  void destruction.then(
    () => {
      completed = true;
    },
    () => {
      completed = true;
    },
  );
  await new Promise((resolve) => setTimeout(resolve, 30));
  expect(port.usable).toBe(false);
  expect(completed).toBe(false);
  await expect(port.close()).rejects.toThrow("physical work");
  port.observer.taskExited({ id: 99 });
  await Promise.resolve();
  expect(completed).toBe(false);
  await expect(port.close()).rejects.toThrow("physical work");
  provider.resolve();
  await expect(destruction).rejects.toThrow("wall time limit");
  expect(completed).toBe(true);
  await port.settled();
  await port.close();
});
