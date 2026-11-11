import { describe, expect, test } from "bun:test";
import { start } from "../src/guest/shim.ts";
import { bail, orb, waterfall, type Context, type Declaration } from "../src/guest.ts";

type Call = [string, ...unknown[]];

/** A recording fake of `tembra:orb/host@0.1.0`; `environment` is the running callback's. */
function fakeHost() {
  const calls: Call[] = [];
  let environment = 1n;
  let id = 0;
  const snapshots = new Map<number, string>();
  const host = {
    environment: () => (calls.push(["environment"]), environment),
    log: (level: string, message: string) => void calls.push(["log", level, message]),
    instanceAddress: () => (calls.push(["instanceAddress"]), { key: ["main"], principal: "alice" }),
    root: () => (calls.push(["root"]), environment === 1n ? undefined : 1n),
    contextCheck: () => void calls.push(["contextCheck"]),
    contextPrincipal: () => (calls.push(["contextPrincipal"]), "alice"),
    contextCancel: () => void calls.push(["contextCancel"]),
    signalCancelled: () => (calls.push(["signalCancelled"]), false),
    stateDeclare: (initial: string, version: number, validate: number | undefined) => {
      calls.push(["stateDeclare", initial, version, validate]);
      snapshots.set(++id, initial);
      return id;
    },
    stateRead: (state: { scope?: bigint; id: number }, known: string | undefined) => {
      calls.push(["stateRead", state, known]);
      const value = snapshots.get(state.id)!;
      return known === value
        ? { revision: value, version: 1 }
        : { value, revision: value, version: 1 };
    },
    stateUpdate: (state: { scope?: bigint; id: number }, value: string) => {
      calls.push(["stateUpdate", state, value]);
      snapshots.set(state.id, value);
      return { value, revision: value, version: 1 };
    },
    hookDeclare: (alias: string) => (calls.push(["hookDeclare", alias]), ++id),
    referenceDeclare: (binding: string, key: string[]) => (
      calls.push(["referenceDeclare", binding, key]),
      ++id
    ),
    referenceRelease: (reference: unknown) => void calls.push(["referenceRelease", reference]),
    referenceStatus: (reference: unknown) => (calls.push(["referenceStatus", reference]), "ready"),
    referenceState: () => ({ value: "null", revision: "0", version: 1 }),
    resourceDeclare: (...args: unknown[]) => (calls.push(["resourceDeclare", ...args]), ++id),
    resourceRead: (resource: unknown) => (calls.push(["resourceRead", resource]), slot),
    referenceReady: async () => void calls.push(["referenceReady"]),
    referenceAcquire: async (binding: string, key: string[]) => (
      calls.push(["referenceAcquire", binding, key]),
      ++id
    ),
    wait: async (milliseconds: number) => void calls.push(["wait", milliseconds]),
    select: (reference: unknown) => void calls.push(["select", reference]),
    selectHook: (hook: unknown) => void calls.push(["selectHook", hook]),
  };
  let slot = 0;
  return {
    host,
    calls,
    enter(next: bigint) {
      environment = next;
    },
    slot(value: number) {
      slot = value;
    },
  };
}

const ARITHMETIC = "example:integrated/arithmetic@1.0.0";

function harness(
  definition: object,
  hooks: Record<string, { module: object; method: string }> = {},
) {
  const fake = fakeHost();
  const dependency = {
    double(value: number) {
      fake.calls.push(["double", value]);
      return value * 2;
    },
  };
  const global: Record<string, unknown> = {};
  const shim = start(
    fake.host as never,
    definition as never,
    {
      dependencies: { [ARITHMETIC]: dependency },
      hooks,
    },
    global,
  );
  return { ...fake, shim, global };
}

describe("guest shim lifecycle", () => {
  test("describe reads the definition without running factories or the host", () => {
    let factories = 0;
    const definition = orb({
      name: "calculator",
      apiVersion: 2,
      create() {
        factories++;
        return {};
      },
      hooks: { guard: waterfall(), choice: bail() },
      contributes: { "example:hooks/audit@1.0.0": { priority: 3, handle() {} } },
      services: {
        calculation: {
          create() {
            factories++;
            return {};
          },
          lifetime: "host",
          linger: 5,
          methods: { read: () => 1, resourceValue: () => 2 },
        },
      },
    });
    const { shim, calls } = harness(definition);
    expect(shim.lifecycle.describe()).toEqual({
      name: "calculator",
      apiVersion: 2,
      services: [
        {
          alias: "calculation",
          methods: ["read", "resourceValue"],
          lifetime: "host",
          linger: 5,
          access: false,
        },
      ],
      hooks: [
        { alias: "guard", composition: "waterfall" },
        { alias: "choice", composition: "bail" },
      ],
      contributes: [{ binding: "example:hooks/audit@1.0.0", priority: 3 }],
    });
    expect(factories).toBe(0);
    expect(calls).toEqual([]);
  });

  test("factories declare state, references and resources only while they run", () => {
    let retained: Declaration | undefined;
    const definition = orb({
      name: "calculator",
      apiVersion: 1,
      services: {
        calculation: {
          create(self: Declaration) {
            retained = self;
            const total = self.state(
              { count: 1 },
              { version: 2, validate: (value) => value.count >= 0 },
            );
            const arithmetic = self.get<{ double(value: number): number }>(ARITHMETIC);
            const number = self.resource("number", { open: () => 21 });
            return { total, arithmetic, number };
          },
          methods: {
            sum(
              environment: {
                total: { value: unknown };
                arithmetic: { double(value: number, context?: Context): number };
              },
              context: Context,
              value: number,
            ) {
              return environment.arithmetic.double(value, context);
            },
          },
        },
      },
    });
    const { shim, calls, enter } = harness(definition);
    enter(7n);
    shim.lifecycle.create(7n, "calculation");
    expect(calls.find((call) => call[0] === "stateDeclare")).toEqual([
      "stateDeclare",
      '{"count":1}',
      2,
      1,
    ]);
    expect(calls.find((call) => call[0] === "referenceDeclare")).toEqual([
      "referenceDeclare",
      ARITHMETIC,
      ["main"],
    ]);
    expect(() => retained!.state(0)).toThrow("Declaration factory has ended");
    // Each handle names its issuing environment.
    expect(calls.find((call) => call[0] === "stateRead")?.[1]).toEqual({ scope: 7n, id: 1 });

    // The dependency import follows its selection in the same synchronous turn.
    calls.length = 0;
    const wrapper = shim.exported("example:integrated/calculation@1.0.0", "calculation", {
      sum: "",
    });
    expect(wrapper.sum!(5 as never)).toBe(10);
    const order = calls.map((call) => call[0]).filter((name) => name !== "environment");
    expect(order).toEqual(["select", "double"]);
    expect(calls.find((call) => call[0] === "select")?.[1]).toEqual({ scope: 7n, id: 2 });
  });

  test("factories are synchronous and failures become err strings", () => {
    const { shim } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        create: () => Promise.resolve({}),
        services: {
          broken: {
            create() {
              throw new Error("factory refused");
            },
            methods: {},
          },
        },
      }),
    );
    expect(() => shim.lifecycle.create(1n, undefined)).toThrow("Factories must be synchronous");
    let thrown: unknown;
    try {
      shim.lifecycle.create(2n, "broken");
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBe("factory refused");
  });

  test("validators run on kernel JSON text and refuse invalid state", () => {
    const { shim, calls } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        create: (self: Declaration) => ({
          state: self.state(1, { validate: (value) => value > 0 }),
        }),
      }),
    );
    shim.lifecycle.create(1n, undefined);
    const callback = calls.find((call) => call[0] === "stateDeclare")![3] as number;
    expect(() => shim.lifecycle.validate(callback, "2")).not.toThrow();
    expect(() => shim.lifecycle.validate(callback, "-1")).toThrow("State validation failed");
  });

  test("resources open into value slots and close with a context", async () => {
    const closed: unknown[] = [];
    const { shim, calls, slot } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        create: (self: Declaration) => ({
          number: self.resource("number", {
            open: async (signal) => (signal.throwIfAborted(), 21),
            close: (value, context) => void closed.push([value, typeof context.check]),
          }),
        }),
        services: {
          s: {
            methods: {
              read: (environment: { number: { value: number } }) => environment.number.value,
            },
          },
        },
      }),
    );
    shim.lifecycle.create(1n, undefined);
    const declared = calls.find((call) => call[0] === "resourceDeclare")!;
    const [, name, version, open, close] = declared as [string, string, number, number, number];
    expect([name, version]).toEqual(["number", 1]);
    const value = await shim.lifecycle.open(open);
    slot(value);
    expect(shim.exported("p:k/s", "s", { read: "" }).read!()).toBe(21);
    await shim.lifecycle.close(close, value);
    expect(closed).toEqual([[21, "function"]]);
  });
});

describe("guest shim state", () => {
  test("readables name their retained revision and refuse an omitted changed value", () => {
    const fake = fakeHost();
    const requests: (string | undefined)[] = [];
    let reply: { value?: string; revision: string; version: number } = {
      value: '{"count":1}',
      revision: "1",
      version: 1,
    };
    fake.host.stateRead = ((_state: unknown, known: string | undefined) => {
      requests.push(known);
      return reply;
    }) as never;
    const shim = start(
      fake.host as never,
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            create: (self: Declaration) => ({ count: self.state({ count: 0 }) }),
            methods: {
              read: (environment: { count: { value: unknown } }) => environment.count.value,
            },
          },
        },
      }) as never,
      { dependencies: {}, hooks: {} },
      {},
    );
    shim.lifecycle.create(1n, "s");
    const read = () => shim.exported("p:k/s", "s", { read: "" }).read!();
    const first = read();
    expect(first).toEqual({ count: 1 });
    expect(Object.isFrozen(first)).toBe(true);
    // The host omits an unchanged value; the retained snapshot keeps its identity.
    reply = { revision: "1", version: 1 };
    expect(read()).toBe(first);
    reply = { value: '{"count":2}', revision: "2", version: 1 };
    expect(read()).toEqual({ count: 2 });
    // A changed revision must carry its value.
    reply = { revision: "3", version: 1 };
    expect(() => read()).toThrow(/omitted a changed state value/);
    expect(requests).toEqual([undefined, "1", "1", "2"]);
  });
});

describe("guest shim callbacks", () => {
  test("a synchronous WIT function refuses a Promise implementation", () => {
    const { shim } = harness(
      orb({ name: "x", apiVersion: 1, services: { s: { methods: { read: async () => 1 } } } }),
    );
    expect(() => shim.exported("p:k/s", "s", { read: "" }).read!()).toThrow(
      "s.read is a synchronous WIT function; declare it async in WIT to return a Promise",
    );
  });

  test("author errors reach the runtime unchanged for its err mapping", async () => {
    const rejected = Object.assign(new Error("Rejected input"), { payload: "negative" });
    const { shim } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            methods: {
              checked: () => {
                throw rejected;
              },
              later: async () => {
                throw rejected;
              },
            },
          },
        },
      }),
    );
    const wrapper = shim.exported("p:k/s", "s", { checked: "r", later: "ar" });
    let thrown: unknown;
    try {
      wrapper.checked!();
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBe(rejected);
    await expect(wrapper.later!()).rejects.toBe(rejected);
  });

  test("declaration metadata is read only in its own environment", () => {
    let retained: Declaration | undefined;
    const { shim, calls, enter } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        // An empty root factory never reads metadata eagerly.
        create: () => ({}),
        services: {
          s: {
            create(self: Declaration) {
              retained = self;
              return { key: self.key };
            },
            methods: {
              mine: (environment: { key: readonly string[] }) => environment.key.join("/"),
              owner: () => retained!.principal,
            },
          },
        },
      }),
    );
    enter(1n);
    shim.lifecycle.create(1n, undefined);
    expect(calls.filter((call) => call[0] === "instanceAddress")).toEqual([]);
    enter(2n);
    shim.lifecycle.create(2n, "s");
    const wrapper = shim.exported("p:k/s", "s", { mine: "", owner: "" });
    expect(wrapper.mine!()).toBe("main");
    // A later callback of the owning environment may still read its metadata.
    expect(wrapper.owner!()).toBe("alice");
    enter(3n);
    calls.length = 0;
    expect(() => wrapper.owner!()).toThrow("readable only in its own environment");
    expect(calls.map((call) => call[0])).not.toContain("instanceAddress");
  });

  test("a context retained into another callback is refused before any host call", async () => {
    let retained: Context | undefined;
    const { shim, calls } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            methods: {
              keep: (_environment: unknown, context: Context) => {
                retained = context;
                return context.principal;
              },
              reuse: () => {
                retained!.check();
                return "accepted";
              },
              acquire: async () => (await retained!.get(ARITHMETIC, "a"), "accepted"),
            },
          },
        },
      }),
    );
    const wrapper = shim.exported("p:k/s", "s", { keep: "", reuse: "", acquire: "a" });
    expect(wrapper.keep!()).toBe("alice");
    calls.length = 0;
    expect(() => wrapper.reuse!()).toThrow("Context callback has ended");
    await expect(wrapper.acquire!()).rejects.toThrow("Context callback has ended");
    expect(calls.filter((call) => call[0] !== "environment")).toEqual([]);
  });

  test("a refused dependency argument clears its selection and the next call proceeds", () => {
    const fake = fakeHost();
    const dependency = {
      double(value: number) {
        if (!Number.isInteger(value)) throw new RangeError("s32 argument out of range");
        fake.calls.push(["double", value]);
        return value * 2;
      },
    };
    const shim = start(
      fake.host as never,
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            create: (self: Declaration) => ({
              arithmetic: self.get<{ double(value: number): number }>(ARITHMETIC),
            }),
            methods: {
              twice(environment: { arithmetic: { double(value: number): number } }) {
                let refused = "";
                try {
                  environment.arithmetic.double(0.5);
                } catch (error) {
                  refused = (error as Error).name;
                }
                return `${refused}:${environment.arithmetic.double(3)}`;
              },
            },
          },
        },
      }) as never,
      { dependencies: { [ARITHMETIC]: dependency }, hooks: {} },
      {},
    );
    shim.lifecycle.create(1n, "s");
    fake.calls.length = 0;
    expect(shim.exported("p:k/s", "s", { twice: "" }).twice!()).toBe("RangeError:6");
    const selections = fake.calls.filter((call) => call[0] === "select" || call[0] === "double");
    expect(selections).toEqual([
      ["select", { scope: 1n, id: 1 }],
      ["select", undefined],
      ["select", { scope: 1n, id: 1 }],
      ["double", 3],
    ]);
  });

  test("an asynchronous import is never cleared after its turn", async () => {
    const fake = fakeHost();
    const dependency = { later: async (value: number) => value };
    const shim = start(
      fake.host as never,
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            create: (self: Declaration) => ({
              arithmetic: self.get<{ later(value: number): Promise<number> }>(ARITHMETIC),
            }),
            methods: {
              later: (environment: { arithmetic: { later(value: number): Promise<number> } }) =>
                environment.arithmetic.later(4),
            },
          },
        },
      }) as never,
      { dependencies: { [ARITHMETIC]: dependency }, hooks: {} },
      {},
    );
    shim.lifecycle.create(1n, "s");
    fake.calls.length = 0;
    expect(await shim.exported("p:k/s", "s", { later: "a" }).later!()).toBe(4);
    expect(fake.calls.filter((call) => call[0] === "select")).toEqual([
      ["select", { scope: 1n, id: 1 }],
    ]);
  });

  test("hooks select the declared hook, then call the typed self-import", () => {
    const fakeModule = {
      apply(input: number) {
        order.push(["apply", input]);
        return input + 1;
      },
    };
    const order: unknown[] = [];
    const definition = orb({
      name: "tools",
      apiVersion: 1,
      hooks: { guard: waterfall() },
      services: {
        runner: {
          create: (self: Declaration) => ({ guard: self.hook<number, number>("guard") }),
          methods: {
            run: (
              hooks: { guard: { run(context: Context, input: number): number } },
              context: Context,
              input: number,
            ) => hooks.guard.run(context, input),
          },
        },
      },
    });
    const { shim, calls } = harness(definition, { guard: { module: fakeModule, method: "apply" } });
    shim.lifecycle.create(1n, "runner");
    calls.length = 0;
    expect(shim.exported("p:k/runner", "runner", { run: "" }).run!(4 as never)).toBe(5);
    const names = calls.map((call) => call[0]).filter((name) => name !== "environment");
    expect(names).toEqual(["contextCheck", "selectHook"]);
    expect(order).toEqual([["apply", 4]]);
  });

  test("contributions run with the root environment", () => {
    const { shim, enter } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        create: () => ({ base: 40 }),
        contributes: {
          "example:hooks/guard@1.0.0": (root: { base: number }, _context: Context, input: number) =>
            root.base + input,
        },
      }),
    );
    enter(1n);
    shim.lifecycle.create(1n, undefined);
    expect(
      shim.exported("example:hooks/guard@1.0.0", "guard", { apply: "" }).apply!(2 as never),
    ).toBe(42);
  });

  test("a byte-stream result keeps its callback epoch until produced", async () => {
    const seen: string[] = [];
    const { shim } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            methods: {
              frames: async (_environment: unknown, context: Context) =>
                (async function* () {
                  seen.push(context.principal);
                  yield Uint8Array.of(1);
                })(),
            },
          },
        },
      }),
    );
    const stream = (await shim.exported("p:k/s", "s", { frames: "as" })
      .frames!()) as AsyncIterable<Uint8Array>;
    const chunks: Uint8Array[] = [];
    for await (const chunk of stream) chunks.push(chunk);
    expect(chunks).toEqual([Uint8Array.of(1)]);
    expect(seen).toEqual(["alice"]);
  });

  test("console is refused during module evaluation and logs inside callbacks", () => {
    const { shim, global, calls } = harness(
      orb({
        name: "x",
        apiVersion: 1,
        services: {
          s: {
            methods: {
              log: () => (globalThis.console, (global.console as Console).warn("method", 1)),
            },
          },
        },
      }),
    );
    const console = global.console as Console;
    expect(() => console.info("module")).toThrow("module evaluation has no host");
    shim.exported("p:k/s", "s", { log: "" }).log!();
    expect(calls.find((call) => call[0] === "log")).toEqual(["log", "warn", "method 1"]);
    expect(typeof (global.tembra as { wait: unknown }).wait).toBe("function");
  });
});
