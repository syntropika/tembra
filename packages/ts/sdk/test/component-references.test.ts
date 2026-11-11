// Browser reference views: generated spellings, the host resource class and the scope guards.
// Real JCO glue and Chrome execution are qualified by `prepared-component-browser.ts`.
import { describe, expect, test } from "bun:test";
import type { Declaration } from "../src/authoring/declaration.ts";
import {
  bindingName,
  referenceImport,
  referenceView,
  type ReferenceView,
} from "../src/interpreter/component/host.ts";
import type { Callback, ComponentPort } from "../src/interpreter/component/port.ts";
import type { Described } from "../src/interpreter/component/revision.ts";
import type { ReferenceBinding } from "../src/interpreter/types.ts";
import {
  ComponentScope,
  type CallbackScope,
  type ReferenceClaim,
  type ScopeOwner,
} from "../src/interpreter/component/scope.ts";
import { converters, type GraphFunction } from "../src/interpreter/component/values.ts";
import type { Context } from "../src/runtime/context.ts";

const BINDING = "example:keyed/counter-ref@1.0.0";
const VIEW: ReferenceBinding = {
  interface: "example:keyed/counter@1.0.0",
  resource: { interface: BINDING, name: "wide-counter" },
  acquire: "get",
  methods: {
    "[method]wide-counter.add": "add",
    "[method]wide-counter.total-of": "total-of",
  },
  signature: "0".repeat(64),
};
// 0: the resource, 1: borrow, 2: own, 3: list<string>, 4: result<own, string>, 5: list<u64>.
const graph: Described = {
  javascriptNames: { get: "get", "[method]wide-counter.add": "methodWideCounterAdd" },
  types: [
    { id: 0, definition: { kind: "resource" } },
    { id: 1, definition: { kind: { handle: { borrow: 0 } } } },
    { id: 2, definition: { kind: { handle: { own: 0 } } } },
    { id: 3, definition: { kind: { list: "string" } } },
    { id: 4, definition: { kind: { result: { ok: 2, err: "string" } } } },
    { id: 5, definition: { kind: { list: "u64" } } },
  ] as never,
  imports: [
    {
      binding: BINDING,
      contract: BINDING,
      functions: {
        get: { params: [{ name: "key", type: 3 }], result: 4 },
        "[method]wide-counter.add": {
          params: [
            { name: "self", type: 1 },
            { name: "amount", type: "u32" },
          ],
          result: "u32",
        },
        "[method]wide-counter.total-of": {
          params: [
            { name: "self", type: 1 },
            { name: "values", type: 5 },
          ],
          result: "u64",
        },
      },
    },
  ],
  exports: [],
  references: { [BINDING]: VIEW },
};
const convert = converters(graph);

describe("reference view preparation", () => {
  test("generated spellings follow kebab-case words", () => {
    expect(bindingName("counter", true)).toBe("Counter");
    expect(bindingName("wide-counter", true)).toBe("WideCounter");
    expect(bindingName("total-of", false)).toBe("totalOf");
    expect(bindingName("get-HTTP-v2", false)).toBe("getHttpV2");
  });

  test("methods convert only their data parameters; the acquisition only its key", () => {
    const view = referenceView(graph, BINDING, VIEW, convert);
    expect(view.resource).toBe("WideCounter");
    expect(view.acquire.spelling).toBe("get");
    expect(view.acquire.values.hostArguments([["a", "b"]])).toEqual([["a", "b"]]);
    expect(view.methods.map(({ name, spelling }) => [name, spelling])).toEqual([
      ["[method]wide-counter.add", "add"],
      ["[method]wide-counter.total-of", "totalOf"],
    ]);
    const total = view.methods[1]!.values;
    expect(total.hostArguments([BigUint64Array.of(1n, 2n)])).toEqual([[1n, 2n]]);
    expect(() => total.hostArguments([{}, BigUint64Array.of(1n)])).toThrow("argument count");
  });

  test("a handle anywhere but a checked receiver or acquisition is refused", () => {
    // Without the role, the identical interface carries handles as data.
    const plain = graph.imports[0]!.functions!["[method]wide-counter.add"]!;
    expect(() => convert(plain as GraphFunction)).toThrow("unavailable");
    const extra: Described = {
      ...graph,
      imports: [
        {
          ...graph.imports[0]!,
          functions: {
            ...graph.imports[0]!.functions,
            leak: { params: [{ name: "value", type: 2 }] },
          },
        },
      ],
    };
    expect(() => referenceView(extra, BINDING, VIEW, convert)).toThrow("unchecked functions");
  });
});

/** A callback scope recording what the view asked of it. */
function recorder(acquired: () => Promise<ReferenceClaim>) {
  const calls: unknown[][] = [];
  const released: ReferenceClaim[] = [];
  const signals: AbortSignal[] = [];
  const scope: CallbackScope = {
    check() {},
    cancel() {},
    call: () => Promise.reject(new Error("plain call")),
    acquire: (binding, key, abandoned) => {
      calls.push(["acquire", binding, key]);
      signals.push(abandoned);
      return acquired();
    },
    invoke: async (claim, binding, method, args) => {
      calls.push(["invoke", claim, binding, method, args]);
      return method.endsWith("total-of") ? 3n : 7;
    },
    release: (claim) => void released.push(claim),
  };
  const callback = { scope, pending: new Set() } as unknown as Callback;
  const cleanups: Promise<unknown>[] = [];
  const port = {
    current: () => callback,
    physical: <T>(_: Callback, operation: Promise<T>) => operation,
    wasi: {
      cleanup: (operation: () => unknown) => cleanups.push(Promise.resolve().then(operation)),
    },
  } as unknown as ComponentPort;
  return { calls, released, signals, port, callback, cleanups };
}

type View = {
  add(amount: number): Promise<number>;
  totalOf(values: BigUint64Array): Promise<bigint>;
  [Symbol.dispose](): void;
};

describe("reference view host class", () => {
  const prepared: ReferenceView = referenceView(graph, BINDING, VIEW, convert);
  const imported = (port: ComponentPort) =>
    referenceImport(port, BINDING, prepared) as {
      WideCounter: new (claim: ReferenceClaim) => View;
      get(key: string[]): Promise<{ tag: string; val: unknown }> & Disposable;
    };

  test("acquisition answers a handle whose methods target exactly its claim", async () => {
    const claim = Object.freeze({ binding: BINDING });
    const run = recorder(async () => claim);
    const { WideCounter, get } = imported(run.port);
    const result = await get(["a"]);
    expect(result.tag).toBe("ok");
    const view = result.val as View;
    expect(view).toBeInstanceOf(WideCounter);
    expect(await view.add(2)).toBe(7);
    expect(await view.totalOf(BigUint64Array.of(1n))).toBe(3n);
    expect(run.calls).toEqual([
      ["acquire", BINDING, ["a"]],
      ["invoke", claim, BINDING, "[method]wide-counter.add", [2]],
      ["invoke", claim, BINDING, "[method]wide-counter.total-of", [[1n]]],
    ]);
  });

  test("a refused acquisition is the WIT err, never a trap", async () => {
    const run = recorder(() => Promise.reject(new Error("refused key")));
    expect(await imported(run.port).get(["refused"])).toEqual({ tag: "err", val: "refused key" });
  });

  test("dropping a handle releases its claim once and kills the handle", async () => {
    const claim = Object.freeze({ binding: BINDING });
    const run = recorder(async () => claim);
    const view = (await imported(run.port).get(["a"])).val as View;
    view[Symbol.dispose]();
    view[Symbol.dispose]();
    // Nothing runs inside the guest's drop; the release follows, accounted to the callback.
    expect(run.released).toEqual([]);
    await Promise.all(run.cleanups);
    expect(run.released).toEqual([claim]);
    expect(() => view.add(1)).toThrow("not live");
  });

  test("generated look-alikes and handles of another import are not live", () => {
    const run = recorder(async () => Object.freeze({ binding: BINDING }));
    const { WideCounter } = imported(run.port);
    const forged = Object.create(WideCounter.prototype) as View;
    expect(() => forged.add(1)).toThrow("not live");
    const other = imported(run.port).WideCounter;
    expect(other).not.toBe(WideCounter);
  });

  test("an abandoned acquisition stops readiness and releases an undelivered claim", async () => {
    const claim = Object.freeze({ binding: BINDING });
    const ready = Promise.withResolvers<ReferenceClaim>();
    const run = recorder(() => ready.promise);
    const pending = imported(run.port).get(["slow"]);
    pending[Symbol.dispose]();
    expect(run.signals[0]!.aborted).toBe(true);
    ready.resolve(claim);
    expect((await pending).tag).toBe("err");
    expect(run.released).toEqual([claim]);

    const late = recorder(async () => claim);
    const delivered = imported(late.port).get(["late"]);
    await delivered;
    // Ready but disposed before the bindings delivered it: the claim is released.
    delivered[Symbol.dispose]();
    expect(late.released).toEqual([claim]);
  });
});

/** A session double: claims, readiness, root lists and physical calls. */
function session() {
  const released: string[] = [];
  const invoked: unknown[][] = [];
  const records = new Map<object, { released: boolean; methods: Set<string>; key: string }>();
  const rootReferences = new Map<number, Set<number>>();
  const ready = new Map<string, Promise<void>>();
  const value = {
    rootReferences,
    report() {},
    scopes: {
      requirement: (_: unknown, service: string) => ({ service, apiVersion: 1, methods: ["add"] }),
    },
    handles: {
      hold(_service: string, key: readonly string[], _options: unknown, context: Context) {
        const name = key.join("/");
        const reference = {
          ready: ready.get(name) ?? Promise.resolve(),
          release() {
            const record = records.get(reference)!;
            if (record.released) return;
            record.released = true;
            released.push(name);
          },
        };
        records.set(reference, { released: false, methods: new Set(["add"]), key: name });
        const owned = rootReferences.get(context.root) ?? new Set();
        owned.add(owned.size + 1);
        rootReferences.set(context.root, owned);
        return reference;
      },
      reference: (reference: object) => records.get(reference)!,
    },
    calls: {
      observe: <T>(physical: Promise<T>, signal: AbortSignal) =>
        new Promise<T>((resolve, reject) => {
          if (signal.aborted) return reject(signal.reason);
          signal.addEventListener("abort", () => reject(signal.reason), { once: true });
          physical.then(resolve, reject);
        }),
      invokePhysical: async (
        record: { key: string },
        method: string,
        args: unknown[],
        context: Context,
      ) => {
        invoked.push([record.key, method, args, context.root]);
        return 1;
      },
    },
  };
  return { value, released, invoked, ready };
}

describe("component scope reference views", () => {
  const SERVICE = "keyed::example:keyed/counter@1.0.0";
  const PLAIN = "example:plain/plain@1.0.0";
  const setup = () => {
    const runtime = session();
    const declared: string[] = [];
    const owner: ScopeOwner = {
      contract: {
        bind: () => 0,
        validate: () => '{"ok":null}',
        request: () => "{}",
      },
      imports: new Map([
        [BINDING, SERVICE],
        [PLAIN, "plain::example:plain/plain@1.0.0"],
      ]),
      importMethods: new Map(),
      views: new Map([[BINDING, new Map([["[method]counter.add", { name: "add", slot: 1 }]])]]),
      importSignatures: new Map(),
      signatures: new Map(),
      scopes: new Map(),
    };
    const declaration = {
      environment: { id: 1, orb: {} },
      session: runtime.value,
      get: (name: string) => {
        declared.push(name);
        return {};
      },
    } as unknown as Declaration;
    const scope = new ComponentScope(owner, declaration, null);
    const context = (root: number, invocation = true) =>
      ({
        root,
        signal: new AbortController().signal,
        ...(invocation ? { invocation: { id: root } } : {}),
        check() {},
      }) as unknown as Context;
    return { runtime, declared, scope, context, owner, declaration };
  };

  test("only plain imports declare a default main reference", () => {
    const { declared } = setup();
    expect(declared).toEqual(["plain::example:plain/plain@1.0.0"]);
  });

  test("acquisition requires an admitted invocation and selects the claim's root", async () => {
    const { scope, context, runtime } = setup();
    const never = new AbortController().signal;
    await expect(scope.exchange(context(1, false)).acquire(BINDING, ["a"], never)).rejects.toThrow(
      "admitted invocation",
    );
    await expect(scope.exchange(context(1)).acquire(PLAIN, ["a"], never)).rejects.toThrow(
      "not a reference view",
    );
    const alice = scope.exchange(context(1));
    const claim = await alice.acquire(BINDING, ["a"], never);
    expect(await alice.invoke(claim, BINDING, "[method]counter.add", [2])).toBe(1);
    expect(runtime.invoked).toEqual([["a", "add", [2], 1]]);
    await expect(alice.invoke(claim, PLAIN, "[method]counter.add", [2])).rejects.toThrow(
      "Foreign reference handle",
    );
    await expect(
      alice.invoke(Object.freeze({ binding: BINDING }), BINDING, "[method]counter.add", [2]),
    ).rejects.toThrow("Foreign reference handle");
    // A later root, even of the same Orb, cannot use or release the claim.
    const bob = scope.exchange(context(2));
    await expect(bob.invoke(claim, BINDING, "[method]counter.add", [2])).rejects.toThrow(
      "another root",
    );
    bob.release(claim);
    expect(runtime.released).toEqual([]);
    alice.release(claim);
    alice.release(claim);
    expect(runtime.released).toEqual([]);
    alice.cancel();
    alice.cancel();
    expect(runtime.released).toEqual(["a"]);
    await expect(
      scope.exchange(context(1)).invoke(claim, BINDING, "[method]counter.add", [2]),
    ).rejects.toThrow("released");
  });

  test("a claim expires with its root", async () => {
    const { scope, context, runtime } = setup();
    const callback = scope.exchange(context(3));
    const claim = await callback.acquire(BINDING, ["a"], new AbortController().signal);
    runtime.value.rootReferences.delete(3);
    await expect(callback.invoke(claim, BINDING, "[method]counter.add", [1])).rejects.toThrow(
      "expired with its root",
    );
  });

  test("abandonment or callback end stops readiness and releases the claim", async () => {
    const { scope, context, runtime } = setup();
    runtime.ready.set("slow", new Promise(() => {}));
    const abandoned = new AbortController();
    const pending = scope.exchange(context(4)).acquire(BINDING, ["slow"], abandoned.signal);
    abandoned.abort(new Error("acquisition abandoned"));
    await expect(pending).rejects.toThrow("abandoned");
    expect(runtime.released).toEqual(["slow"]);

    const callback = scope.exchange(context(5));
    const ended = callback.acquire(BINDING, ["slow"], new AbortController().signal);
    callback.cancel();
    await expect(ended).rejects.toThrow("scope ended");
    expect(runtime.released).toEqual(["slow", "slow"]);
    await expect(callback.acquire(BINDING, ["a"], abandoned.signal)).rejects.toThrow("scope ended");
  });
});
