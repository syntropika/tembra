import { describe, expect, test } from "bun:test";
import {
  ResourceOwner,
  resourceRoles,
  resourceDataFunction,
  resourceExport,
} from "../src/interpreter/component/resources.ts";
import { resourceImport } from "../src/interpreter/component/host.ts";
import {
  converters,
  type GraphFunction,
  type TypeGraph,
} from "../src/interpreter/component/values.ts";
import {
  ComponentScope,
  type ScopeOwner,
  type CallbackScope,
} from "../src/interpreter/component/scope.ts";
import type { Declaration } from "../src/authoring/declaration.ts";
import type { Context } from "../src/runtime/context.ts";
import { ComponentRevision, type Described } from "../src/interpreter/component/revision.ts";
import { encode, Encoded } from "../src/interpreter/codec.ts";
import { ComponentPort } from "../src/interpreter/component/port.ts";
import type { Session } from "../src/runtime/session.ts";
import type { ReferenceRecord } from "../src/runtime/records.ts";
import { createHost } from "../src/node.ts";
import { orb } from "../src/orb.ts";
import { Code, TembraError } from "../src/index.ts";
import type { Host } from "../src/host.ts";
import { contract, host, refusal, type Compiled } from "./compiled-components.ts";

const binding = "test:resources/sessions@1.0.0";
const nominal = { interface: binding, name: "session" };
const graph: TypeGraph = {
  javascriptNames: {},
  types: [
    { id: 0, definition: { kind: "resource", owner: binding, name: "session" } },
    { id: 1, definition: { kind: { handle: { own: 0 } } } },
    { id: 2, definition: { kind: { handle: { borrow: 0 } } } },
    { id: 3, definition: { kind: { type: 1 } } },
  ],
};
const constructor: GraphFunction = {
  name: "[constructor]session",
  kind: { constructor: 0 },
  params: [{ name: "seed", type: "u64" }],
  result: 3,
};
const add: GraphFunction = {
  name: "[method]session.add",
  kind: { method: 0 },
  params: [
    { name: "self", type: 2 },
    { name: "n", type: "u64" },
  ],
  result: "u64",
};
const own = () => {
  const issuer = {};
  const value = {};
  let destroyed = 0;
  const owner = new ResourceOwner({ ...nominal }, issuer, 7, value, () => {
    destroyed++;
  });
  return { owner, issuer, value, destroyed: () => destroyed };
};

describe("compiled resource data positions", () => {
  test("whole owned results and implicit self use a separate capability lane", () => {
    const roles = resourceRoles(graph, binding, constructor);
    expect(roles.owned).toEqual([nominal]);
    expect(roles.construct).toBe(true);
    expect(resourceDataFunction(constructor, roles).result).toBe(constructor.result);
    const method = resourceRoles(graph, binding, add);
    expect(method.receiver).toEqual(nominal);
    expect(resourceDataFunction(add, method).params).toEqual([{ name: "n", type: "u64" }]);
    expect(() => converters(graph)(add)).toThrow("unavailable");
    expect(() => resourceRoles(graph, "test:foreign/sessions@1.0.0", constructor)).toThrow(
      "defining interface",
    );
  });
  test("guest resource methods run on exactly their instance", async () => {
    class Session {
      total: bigint;
      constructor(seed: bigint) {
        this.total = seed;
      }
      add(n: bigint) {
        return (this.total += n);
      }
    }
    const made = resourceExport(
      { Session },
      "constructorSession",
      resourceRoles(graph, binding, constructor),
      undefined,
      [3n],
    ) as Session;
    expect(
      resourceExport({ Session }, "methodSessionAdd", resourceRoles(graph, binding, add), made, [
        2n,
      ]),
    ).toBe(5n);
  });
});

describe("provider-owned capability lifetime", () => {
  test("issuer, environment and versioned nominal identity all must match", () => {
    const run = own();
    expect(run.owner.redeem(run.issuer, 7, nominal)).toBe(run.value);
    expect(() => run.owner.redeem({}, 7, nominal)).toThrow("another instance");
    expect(() => run.owner.redeem(run.issuer, 8, nominal)).toThrow("another instance");
    expect(() =>
      run.owner.redeem(run.issuer, 7, { ...nominal, interface: binding.replace("1.0.0", "2.0.0") }),
    ).toThrow("nominal");
    run.owner.close();
    expect(() => run.owner.redeem(run.issuer, 7, nominal)).toThrow("another instance");
    expect(run.destroyed()).toBe(1);
  });
  test("drop revokes new calls but suspended physical borrows keep their resource alive", () => {
    const run = own();
    const end = run.owner.lend();
    run.owner.close();
    run.owner.close();
    expect(run.destroyed()).toBe(0);
    expect(() => run.owner.lend()).toThrow("not live");
    expect(run.owner.redeem(run.issuer, 7, nominal)).toBe(run.value);
    end();
    end();
    expect(run.destroyed()).toBe(1);
  });
  test("guest drops wait for the importing callback's physical end", () => {
    const run = own();
    const owner = {
      imports: new Map(),
      views: new Map(),
      scopes: new Map(),
    } as unknown as ScopeOwner;
    const declaration = {
      environment: { id: 1 },
      session: { report() {} },
    } as unknown as Declaration;
    const environment = new ComponentScope(owner, declaration, null);
    const callback = environment.exchange({ check() {} } as unknown as Context);
    callback.releaseResource(run.owner);
    callback.releaseResource(run.owner);
    expect(run.destroyed()).toBe(0);
    callback.cancel();
    callback.cancel();
    expect(run.destroyed()).toBe(1);
  });
  test("the exact duplicated claim is released only after physical destruction", async () => {
    const done = Promise.withResolvers<void>();
    const released: number[] = [];
    const original = { id: 1 } as ReferenceRecord;
    const copy = { id: 2 } as ReferenceRecord;
    const session = {
      handles: {
        duplicate: (record: ReferenceRecord) => {
          expect(record).toBe(original);
          return copy;
        },
        release: (id: number) => released.push(id),
      },
    } as unknown as Session;
    const owner = new ResourceOwner(nominal, {}, 7, {}, () => done.promise);
    owner.anchor(session, original);
    expect(owner.reference).toBe(copy);
    expect(() => owner.anchor(session, original)).toThrow("already delivered");
    owner.close();
    expect(released).toEqual([]);
    done.resolve();
    await done.promise;
    expect(released).toEqual([2]);
  });
});

describe("ordinary imported resource classes", () => {
  const setup = (factory: () => Promise<ResourceOwner>) => {
    const calls: unknown[] = [];
    const callback = {
      ended: new AbortController(),
      pending: new Set(),
      scope: {
        releaseResource: (owner: ResourceOwner) => owner.close(),
        call: async (...args: unknown[]) => {
          calls.push(args);
          return factory();
        },
        resource: async (owner: ResourceOwner, ...args: unknown[]) => {
          calls.push([owner, ...args]);
          return 9n;
        },
      },
    };
    const port = {
      current: () => callback,
      physical: (_: unknown, pending: Promise<unknown>) => pending,
    } as unknown as ComponentPort;
    const convert = converters(graph);
    const functions = [constructor, add].map((fn) => {
      const roles = resourceRoles(graph, binding, fn);
      return {
        name: fn.name!,
        spelling: fn === constructor ? "constructorSession" : "methodSessionAdd",
        roles,
        values: convert(resourceDataFunction(fn, roles)),
      };
    });
    const imports = resourceImport(port, binding, functions) as {
      Session: new (seed: bigint) => Promise<{ add(n: bigint): Promise<bigint> } & Disposable>;
    };
    return { imports, calls };
  };
  test("each constructor owns a fresh provider instance; drops close exactly once", async () => {
    const run = own();
    const { imports, calls } = setup(async () => run.owner);
    const resource = await new imports.Session(3n);
    expect(await resource.add(2n)).toBe(9n);
    expect(calls[1]).toEqual([run.owner, binding, "methodSessionAdd", [2n]]);
    resource[Symbol.dispose]();
    resource[Symbol.dispose]();
    expect(run.destroyed()).toBe(1);
    expect(() => resource.add(1n)).toThrow("not live");
  });
  test("an abandoned asynchronous owned answer closes when it finally arrives", async () => {
    const run = own();
    const ready = Promise.withResolvers<ResourceOwner>();
    const { imports } = setup(() => ready.promise);
    const pending = new imports.Session(3n) as Promise<unknown> & Disposable;
    pending[Symbol.dispose]();
    ready.resolve(run.owner);
    await expect(pending).rejects.toThrow("abandoned");
    expect(run.destroyed()).toBe(1);
  });
});

test("an issued resource dropped while its provider is busy waits outside that callback", async () => {
  let task = 1;
  class ObservedPort extends ComponentPort {
    override run<T>(environment: bigint, scope: CallbackScope, call: () => T | Promise<T>) {
      return super.run(environment, scope, () => {
        const id = task++;
        this.observer.taskStarted({ id, entry: "test", isAsync: true });
        const result = Promise.resolve().then(call);
        result.then(
          () => queueMicrotask(() => this.observer.taskExited({ id })),
          (error) => this.observer.taskExited({ id, rejected: true, error }),
        );
        return result;
      });
    }
  }
  const scope = (): CallbackScope => ({
    check() {},
    cancel() {},
    report() {},
    releaseResource() {},
    release() {},
    call: async () => null,
    acquire: async () => {
      throw new Error("unused");
    },
    invoke: async () => {
      throw new Error("unused");
    },
  });
  const port = new ObservedPort(200, async () => {});
  let destroyed = 0;
  port.exports = {
    [binding]: {
      open: () => ({
        [Symbol.dispose]() {
          destroyed++;
        },
      }),
    },
  };
  const revision = new ComponentRevision(
    port,
    {
      validate: () => '{"ok":null}',
      bind: () => 0,
      request: () => "{}",
    },
    { ...graph, imports: [], exports: [], references: {} } as Described,
    200,
  );
  const environment = { id: 7 } as ComponentScope;
  revision.scopes.set(environment.id, environment);
  const fn: GraphFunction = { name: "open", kind: "freestanding", params: [], result: 1 };
  const roles = resourceRoles(graph, binding, fn);
  const output = await revision["invoke"](
    binding,
    "open",
    converters(graph)(resourceDataFunction(fn, roles)),
    0,
    { params: undefined, result: undefined },
    environment,
    scope(),
    encode([]),
    roles,
  );
  if (output instanceof Encoded || !output.capabilities?.length)
    throw new Error("Missing issued resource");
  const unlock = await revision.gate.enter();
  const held = Promise.withResolvers<void>();
  const active = port.run(7n, scope(), () => held.promise).finally(unlock);
  output.capabilities[0]!.close();
  held.resolve();
  await active;
  await port.settled();
  expect(destroyed).toBe(1);
  expect(port.usable).toBe(true);
  await port.close();
});

describe("nested capability result admission", () => {
  const nested: TypeGraph = {
    javascriptNames: { first: "first", rest: "rest" },
    types: [
      ...graph.types,
      { id: 4, definition: { kind: { list: 1 } } },
      {
        id: 5,
        definition: {
          kind: {
            record: {
              fields: [
                { name: "first", type: 1 },
                { name: "rest", type: 4 },
              ],
            },
          },
        },
      },
    ],
  };
  const fn: GraphFunction = { name: "open", kind: "freestanding", params: [], result: 5 };
  const roles = resourceRoles(nested, binding, fn);

  test("the same WIT mapper traverses containers without touching ordinary data", () => {
    const seen: unknown[] = [];
    const mapped = roles.result!({ first: "a", rest: ["b", "c"] }, (type, value) => {
      expect(type).toEqual(nominal);
      seen.push(value);
      return seen.length - 1;
    });
    expect(roles.owned).toEqual([nominal]);
    expect(seen).toEqual(["a", "b", "c"]);
    expect(mapped).toEqual({ first: 0, rest: [1, 2] });
  });

  test("duplicate, unused, mistyped and cancelled batches close every attachment", async () => {
    for (const variant of ["duplicate", "unused", "mistyped", "cancelled"] as const) {
      const first = own();
      const second = own();
      if (variant === "mistyped") Object.assign(second.owner.nominal, { name: "other" });
      const importedResources = new Set<ResourceOwner>();
      const reference = { methods: new Set(["open"]) };
      let cancelled = false;
      const owner = {
        contract: { validate: () => '{"ok":null}' },
        imports: new Map([[binding, "provider"]]),
        importMethods: new Map([[binding, new Map([["open", { name: "open", slot: 0, roles }]])]]),
        importSignatures: new Map([[binding, "nested-signature"]]),
        signatures: new Map([[0, { params: undefined, result: undefined }]]),
        views: new Map(),
        scopes: new Map(),
        importedResources,
      } as unknown as ScopeOwner;
      const declaration = {
        environment: { id: 1 },
        get: () => reference,
        session: {
          handles: { reference: () => reference },
          calls: {
            invokePhysical: async (
              _record: unknown,
              _name: unknown,
              _args: unknown,
              _context: unknown,
              capture: { output: Encoded; capabilities: ResourceOwner[] },
            ) => {
              capture.output = encode({
                first: 0,
                rest: variant === "unused" ? [] : [variant === "duplicate" ? 0 : 1],
              });
              capture.capabilities = [first.owner, second.owner];
              cancelled = variant === "cancelled";
            },
          },
          report() {},
        },
      } as unknown as Declaration;
      const context = {
        root: 1,
        check() {
          if (cancelled) throw new Error("cancelled");
        },
      } as unknown as Context;
      const scope = new ComponentScope(owner, declaration, null).exchange(context);
      await expect(scope.call(binding, "open", [])).rejects.toThrow();
      expect(first.destroyed()).toBe(1);
      expect(second.destroyed()).toBe(1);
      expect(importedResources.size).toBe(0);
      if (variant === "mistyped") Object.assign(second.owner.nominal, nominal);
    }
  });
});

const CHAIN = `package test:chain@1.0.0;
interface probe {
  ping: func() -> u32;
}
world chain {
  import tembra:guest/resources@1.0.0;
  export tembra:guest/resources-init@1.0.0;
  export probe;
}`;

/**
 * A compiled Orb whose root lists `declarations` and records each open and close in `events`;
 * the opener of each resource named in `refused` answers `err` with its message the first time.
 */
const chain = async (
  declarations: readonly { name: string; dependencies: string[] }[],
  events: string[],
  refused: Readonly<Record<string, string>> = {},
): Promise<Compiled> => ({
  name: "chain",
  contract: await contract(CHAIN, "chain"),
  component: () => ({
    "tembra:guest/resources-init@1.0.0": {
      declare: () => declarations.map((declaration) => ({ ...declaration, version: 1 })),
      open: (name: string) => {
        events.push(`open ${name}`);
        if (name in refused && events.filter((event) => event === `open ${name}`).length === 1)
          throw refusal(refused[name]!);
        return 0;
      },
      close: (name: string) => void events.push(`close ${name}`),
    },
    "test:chain/probe@1.0.0": { ping: () => 0 },
  }),
});

describe("dependencies of a compiled Orb's root resources", () => {
  test("`c` depends on `b`, `b` on `a`: they open `a`, `b`, `c` and close `c`, `b`, `a`", async () => {
    const events: string[] = [];
    const owner = host([
      await chain(
        [
          { name: "a", dependencies: [] },
          { name: "b", dependencies: ["a"] },
          { name: "c", dependencies: ["b"] },
        ],
        events,
      ),
    ]);
    await owner.install({ artifact: Uint8Array.of(0) });
    const probe = await owner.get("chain::test:chain/probe@1.0.0", "main");
    expect(await probe.ping()).toBe(0);
    probe.release();
    await owner.close();
    expect(events).toEqual(["open a", "open b", "open c", "close c", "close b", "close a"]);
    expect(owner.errors()).toEqual([]);
  });

  test("a dependency not declared before its resource refuses the Orb, naming both", async () => {
    const events: string[] = [];
    const owner = host([
      await chain(
        [
          { name: "a", dependencies: [] },
          { name: "b", dependencies: ["c"] },
          { name: "c", dependencies: [] },
        ],
        events,
      ),
    ]);
    try {
      const installed = owner
        .install({ artifact: Uint8Array.of(0) })
        .then(() => owner.get("chain::test:chain/probe@1.0.0", "main"));
      await expect(installed).rejects.toThrow("the resource `b` depends on `c`");
      expect(events).toEqual([]);
    } finally {
      await owner.close();
    }
  });
});

describe("a root resource that does not open", () => {
  /**
   * Install, through `owner`, an Orb whose root opens `a` and then fails to open `b`: the
   * installation fails with the failed activation's own error, `failed`, unchanged, and `a`
   * closes; a later `get` of `service` activates the root again and fails alike. The host
   * records no error, and no rejection goes unhandled (bun fails the running test on one).
   */
  const refused = async (
    owner: Host,
    install: () => Promise<void>,
    service: string,
    events: string[],
    failed: { code: string; reason: string; message: string },
  ) => {
    try {
      await expect(install()).rejects.toMatchObject(failed);
      expect(events).toEqual(["open a", "open b", "close a"]);
      await expect(owner.get(service, "main")).rejects.toMatchObject(failed);
      expect(events.slice(3)).toEqual(["open a", "open b", "close a"]);
      await owner.close();
      // A rejection nobody handles is reported after the turn that dropped it.
      await new Promise((settled) => setTimeout(settled, 10));
      expect(owner.errors()).toEqual([]);
    } finally {
      await owner.close();
    }
  };

  test("of a compiled Orb fails the waiting call, closes in reverse, and opens again", async () => {
    // Opening `c` answers `err("down")` after `a` and `b` opened, the first time only.
    const events: string[] = [];
    const owner = host([
      await chain(
        [
          { name: "a", dependencies: [] },
          { name: "b", dependencies: ["a"] },
          { name: "c", dependencies: ["b"] },
        ],
        events,
        { c: "down" },
      ),
    ]);
    try {
      await expect(owner.install({ artifact: Uint8Array.of(0) })).rejects.toMatchObject({
        code: Code.Unavailable,
        reason: "resource",
        message: "the resource `c` did not open: down",
      });
      expect(events).toEqual(["open a", "open b", "open c", "close b", "close a"]);
      // The component stays serving: the next get opens all three, and a call answers.
      const probe = await owner.get("chain::test:chain/probe@1.0.0", "main");
      expect(events.slice(5)).toEqual(["open a", "open b", "open c"]);
      expect(await probe.ping()).toBe(0);
      probe.release();
    } finally {
      await owner.close();
    }
    expect(events.slice(8)).toEqual(["close c", "close b", "close a"]);
    // A rejection nobody handles fails the running test; the message lands on the waiter only.
    expect(owner.errors()).toEqual([]);
  });

  test("of a TypeScript Orb fails the waiting calls with the opener's error", async () => {
    const events: string[] = [];
    const owner = createHost();
    const failed = { code: Code.Invalid, reason: "down", message: "the store is down" };
    const definition = orb({
      name: "chain",
      apiVersion: 1,
      create(self) {
        const a = self.resource("a", {
          open: () => void events.push("open a"),
          close: () => void events.push("close a"),
        });
        self.resource("b", {
          deps: [a],
          open: () => {
            events.push("open b");
            throw new TembraError(failed);
          },
          close: () => void events.push("close b"),
        });
        return {};
      },
      services: { probe: { create: () => ({}), methods: { ping: () => 0 } } },
    });
    await refused(owner, () => owner.install(definition), "probe", events, failed);
  });
});

/** The world, whose root owns resources when `root` says so. */
const sockets = (root: boolean) => `package test:sockets@1.0.0;
interface probe {
  /// The handle of this object's \`y\`.
  mine: func() -> u32;
  /// The handle of the root's \`a\`.
  shared: func() -> u32;
}
world sockets {
  import tembra:guest/resources@1.0.0;
  ${root ? "export tembra:guest/resources-init@1.0.0;" : ""}
  export tembra:guest/service-resources-init@1.0.0;
  export probe;
}`;

const PROBE = "test:sockets/probe@1.0.0";

/**
 * A compiled Orb whose root owns `a` when `root` says so and each object of whose service owns
 * `x`, which depends on `a` (on nothing without a root resource), and `y`, which depends on `x`.
 * It records each open and close in `events` and hands out handles in opening order; when `down`
 * says so, the `y` that would get the handle 5 answers `err("down")` instead.
 */
const socketsOrb = async (events: string[], root = true, down = false): Promise<Compiled> => ({
  name: "sockets",
  contract: await contract(sockets(root), "sockets"),
  component: ({ "tembra:guest/resources@1.0.0": resources }) => {
    let next = 0;
    const open = (name: string) => {
      if (down && name === "y" && next === 4) throw refusal("down");
      events.push(`open ${name}`);
      return ++next;
    };
    const close = (name: string) => void events.push(`close ${name}`);
    return {
      ...(root && {
        "tembra:guest/resources-init@1.0.0": {
          declare: () => [{ name: "a", version: 1, dependencies: [] }],
          open,
          close,
        },
      }),
      "tembra:guest/service-resources-init@1.0.0": {
        declare: (service: string) =>
          service === PROBE
            ? [
                { name: "x", version: 1, dependencies: root ? ["a"] : [] },
                { name: "y", version: 1, dependencies: ["x"] },
              ]
            : [],
        open: (service: string, name: string) => {
          if (service !== PROBE) throw new Error(`no resources for ${service}`);
          return open(name);
        },
        close: (service: string, name: string) => {
          if (service !== PROBE) throw new Error(`no resources for ${service}`);
          close(name);
        },
      },
      [PROBE]: {
        mine: () => resources!.read("y"),
        shared: () => resources!.read("a"),
      },
    };
  },
});

/** Wait until `events` holds `count` entries. */
const recorded = async (events: readonly string[], count: number) => {
  for (let turn = 0; events.length < count && turn < 200; turn++)
    await new Promise((settled) => setTimeout(settled, 5));
};

describe("resources of a compiled Orb's service objects", () => {
  type Probe = { mine(): Promise<number>; shared(): Promise<number>; release(): void };
  const get = async (owner: Host, key: string) =>
    (await owner.get(`sockets::${PROBE}`, key)) as unknown as Probe;

  test("each object opens its own after the root's and closes them before", async () => {
    const events: string[] = [];
    const owner = host([await socketsOrb(events)]);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const one = await get(owner, "one");
      const two = await get(owner, "two");
      expect(events).toEqual(["open a", "open x", "open y", "open x", "open y"]);
      // The handles are the opening order: `a` is 1; `one` opened 2 and 3, `two` 4 and 5.
      expect(await one.mine()).toBe(3);
      expect(await two.mine()).toBe(5);
      expect(await one.shared()).toBe(1);
      expect(await two.shared()).toBe(1);
      one.release();
      await recorded(events, 7);
      expect(events.slice(5)).toEqual(["close y", "close x"]);
      expect(await two.mine()).toBe(5);
      two.release();
    } finally {
      await owner.close();
    }
    expect(events.slice(7)).toEqual(["close y", "close x", "close a"]);
    expect(owner.errors()).toEqual([]);
  });

  test("a service owns resources without the root", async () => {
    const events: string[] = [];
    const owner = host([await socketsOrb(events, false)]);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const one = await get(owner, "one");
      expect(await one.mine()).toBe(2);
      one.release();
    } finally {
      await owner.close();
    }
    expect(events).toEqual(["open x", "open y", "close y", "close x"]);
    expect(owner.errors()).toEqual([]);
  });

  test("an object whose open answers an error is not created and closes what opened", async () => {
    const events: string[] = [];
    const owner = host([await socketsOrb(events, true, true)]);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const one = await get(owner, "one");
      await expect(get(owner, "two")).rejects.toMatchObject({
        code: Code.Unavailable,
        reason: "resource",
        message: "the resource `y` did not open: down",
      });
      expect(events).toEqual(["open a", "open x", "open y", "open x", "close x"]);
      expect(await one.mine()).toBe(3);
      expect(await one.shared()).toBe(1);
      one.release();
    } finally {
      await owner.close();
    }
    expect(events.slice(5)).toEqual(["close y", "close x", "close a"]);
    expect(owner.errors()).toEqual([]);
  });
});

const DESKS = `package test:desks@1.0.0;
interface desk {
  /// This object's state.
  stored: func() -> string;
}
interface counter {
  /// The handle of this object's \`w\`.
  mine: func() -> u32;
}
world desks {
  import tembra:guest/state@1.0.0;
  import tembra:guest/resources@1.0.0;
  export tembra:guest/resources-init@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export tembra:guest/service-resources-init@1.0.0;
  export desk;
  export counter;
}`;

const DESK = "test:desks/desk@1.0.0";
const COUNTER = "test:desks/counter@1.0.0";

/**
 * A compiled Orb whose root owns `a` and whose services `desk` and `counter` each keep a state.
 * Each `desk` object lists `desk`'s resources; each `counter` object owns `w`, which depends on
 * `a`. `state-init.declare` tries to read `y`, and each opener of a service resource tries to read
 * the object's state: `seen` records what each answered.
 */
const desks = async (
  desk: readonly { name: string; dependencies: string[] }[],
  events: string[],
  seen: string[],
): Promise<Compiled> => ({
  name: "desks",
  contract: await contract(DESKS, "desks"),
  component: ({ "tembra:guest/state@1.0.0": state, "tembra:guest/resources@1.0.0": resources }) => {
    let next = 0;
    const attempt = (what: string, read: () => unknown) => {
      try {
        seen.push(`${what}: ${String(read())}`);
      } catch (error) {
        seen.push(`${what}: ${(error as Error).message}`);
      }
    };
    const open = (name: string) => {
      events.push(`open ${name}`);
      return ++next;
    };
    return {
      "tembra:guest/resources-init@1.0.0": {
        declare: () => [{ name: "a", version: 1, dependencies: [] }],
        open,
        close: (name: string) => void events.push(`close ${name}`),
      },
      "tembra:guest/state-init@1.0.0": {
        declare: () => {
          attempt("declare", () => resources!.read("y"));
          return { initial: "{}", version: 1, validated: false };
        },
        validate: () => {},
      },
      "tembra:guest/service-resources-init@1.0.0": {
        declare: (service: string) =>
          (service === DESK ? desk : [{ name: "w", dependencies: ["a"] }]).map((resource) => ({
            ...resource,
            version: 1,
          })),
        open: (_service: string, name: string) => {
          attempt(`open ${name}`, () => state!.read());
          return open(name);
        },
        close: (_service: string, name: string) => {
          attempt(`close ${name}`, () => state!.read());
          events.push(`close ${name}`);
        },
      },
      [DESK]: { stored: () => state!.read() },
      [COUNTER]: { mine: () => resources!.read("w") },
    };
  },
});

describe("the kernel's view of a compiled service object while it opens", () => {
  test("an opener and a closer read no state; the state's declaration reads no resource of the object", async () => {
    const events: string[] = [];
    const seen: string[] = [];
    const owner = host([
      await desks(
        [
          { name: "x", dependencies: ["a"] },
          { name: "y", dependencies: ["x"] },
        ],
        events,
        seen,
      ),
    ]);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const desk = (await owner.get(`desks::${DESK}`, "one")) as unknown as {
        stored(): Promise<string>;
        release(): void;
      };
      // An admitted call reads the state the opener could not.
      expect(await desk.stored()).toBe("{}");
      desk.release();
    } finally {
      await owner.close();
    }
    const refused = "this service object declares no kernel state";
    expect(seen).toEqual([
      "declare: neither this object nor the Orb root declares a resource named y",
      `open x: ${refused}`,
      `open y: ${refused}`,
      `close y: ${refused}`,
      `close x: ${refused}`,
    ]);
    expect(events).toEqual(["open a", "open x", "open y", "close y", "close x", "close a"]);
    expect(owner.errors()).toEqual([]);
  });

  test("a dependency the object cannot have refuses that object only, naming both", async () => {
    // Unknown, another service's, and a later one of its own.
    for (const dependency of ["z", "w", "y"]) {
      const events: string[] = [];
      const owner = host([
        await desks(
          [
            { name: "x", dependencies: [dependency] },
            { name: "y", dependencies: [] },
          ],
          events,
          [],
        ),
      ]);
      try {
        await owner.install({ artifact: Uint8Array.of(0) });
        const counter = (await owner.get(`desks::${COUNTER}`, "one")) as unknown as {
          mine(): Promise<number>;
          release(): void;
        };
        await expect(owner.get(`desks::${DESK}`, "one")).rejects.toThrow(
          `the resource \`x\` depends on \`${dependency}\`, which the Orb does not declare before it`,
        );
        // The root and the other service's object keep serving.
        expect(await counter.mine()).toBe(2);
        counter.release();
      } finally {
        await owner.close();
      }
      expect(events).toEqual(["open a", "open w", "close w", "close a"]);
      expect(owner.errors()).toEqual([]);
    }
  });
});
