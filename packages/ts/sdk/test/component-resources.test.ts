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
