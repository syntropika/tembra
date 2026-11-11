import { expect, test } from "bun:test";
import {
  ResourceOwner,
  resourceRoles,
  resourceDataFunction,
} from "../src/interpreter/component/resources.ts";
import { resourceImport, importedResource } from "../src/interpreter/component/host.ts";
import { ComponentPort } from "../src/interpreter/component/port.ts";
import { ComponentRevision, type Described } from "../src/interpreter/component/revision.ts";
import {
  converters,
  type TypeGraph,
  type GraphFunction,
} from "../src/interpreter/component/values.ts";
import type { ComponentScope, CallbackScope } from "../src/interpreter/component/scope.ts";
import type { Session } from "../src/runtime/session.ts";
import type { ReferenceRecord } from "../src/runtime/records.ts";
import { bindCaptured, type CapturedArguments } from "../src/runtime/adapters.ts";
import { createHost } from "../src/node.ts";
import { encode } from "../src/interpreter/codec.ts";

const binding = "test:moves/sessions@1.0.0";
const nominal = { interface: binding, name: "session" };
const graph: TypeGraph = {
  javascriptNames: {},
  types: [
    {
      id: 0,
      definition: { kind: "resource", owner: binding, name: "session" },
    },
    { id: 1, definition: { kind: { handle: { own: 0 } } } },
  ],
};
const transfer = (owners: ResourceOwner[]): CapturedArguments => ({
  owners,
  accepted: false,
  accept() {
    Object.assign(this, { accepted: true });
  },
});

test("move empties stale wrappers while the original claim and outstanding borrow remain physical", async () => {
  const issuer = {};
  const raw = {};
  const done = Promise.withResolvers<void>();
  const released: number[] = [];
  let destroyed = 0;
  let duplicates = 0;
  const claim = { id: 2 } as ReferenceRecord;
  const session = {
    handles: {
      duplicate: () => {
        duplicates++;
        return claim;
      },
      release: (id: number) => released.push(id),
    },
  } as unknown as Session;
  const original = new ResourceOwner(nominal, issuer, 7, raw, () => {
    destroyed++;
    return done.promise;
  });
  original.anchor(session, { id: 1 } as ReferenceRecord);
  const end = original.lend();
  const retire = original.retirement();
  const moved = original.move();
  expect(() => original.reference).toThrow("not live");
  expect(() => original.redeem(issuer, 7, nominal)).toThrow("another instance");
  expect(() => original.move()).toThrow("not live");
  original.close();
  expect(moved.reference).toBe(claim);
  expect(moved.redeem(issuer, 7, nominal)).toBe(raw);
  expect(end.receiver.redeem(issuer, 7, nominal)).toBe(raw);
  expect(duplicates).toBe(1);
  retire();
  expect(destroyed).toBe(0);
  end();
  expect(() => end.receiver.redeem(issuer, 7, nominal)).toThrow("another instance");
  expect(destroyed).toBe(1);
  expect(released).toEqual([]);
  done.resolve();
  await done.promise;
  expect(released).toEqual([2]);
  moved.close();
  expect(destroyed).toBe(1);
});

test("returning an owner to its issuer consumes the host generation without a second destructor", () => {
  const issuer = {};
  const raw = {};
  let destroyed = 0;
  const owner = new ResourceOwner(nominal, issuer, 7, raw, () => {
    destroyed++;
  });
  const retire = owner.retirement();
  const moved = owner.move();
  expect(moved.issuedBy(issuer)).toBe(true);
  expect(() => moved.take({}, nominal)).toThrow("another issuing instance");
  expect(moved.take(issuer, nominal)).toBe(raw);
  expect(() => moved.take(issuer, nominal)).toThrow("not live");
  owner.close();
  moved.close();
  retire();
  expect(destroyed).toBe(0);
});

test("ownership generations unsubscribe on move while the current owner waits for physical destruction", async () => {
  const destroyed = Promise.withResolvers<void>();
  const original = new ResourceOwner(nominal, {}, 7, {}, () => destroyed.promise);
  const ended: string[] = [];
  original.whenClosed(() => ended.push("sender"));
  const first = original.move();
  first.whenClosed(() => ended.push("first recipient"));
  const final = first.move();
  final.whenClosed(() => ended.push("current recipient"));
  expect(ended).toEqual(["sender", "first recipient"]);
  original.close();
  first.close();
  final.close();
  expect(ended).toEqual(["sender", "first recipient"]);
  destroyed.resolve();
  await destroyed.promise;
  expect(ended).toEqual(["sender", "first recipient", "current recipient"]);
});

test("an unaccepted private argument batch closes on refusal before admission and never reaches data handlers", async () => {
  const host = createHost();
  let entered = 0;
  let destroyed = 0;
  await host.install({
    name: "moves",
    apiVersion: 1,
    services: {
      moves: {
        methods: {
          take: () => {
            entered++;
            return 1;
          },
        },
      },
    },
  });
  const reference = await host.get("moves", "main");
  const owner = new ResourceOwner(nominal, {}, 1, {}, () => {
    destroyed++;
  });
  const moved = owner.move();
  try {
    await expect(
      host.root((context) =>
        host.session.calls.invokePhysical(
          host.session.handles.reference(reference),
          "take",
          [0],
          context,
          {
            signature: "moves",
            input: encode([0]),
            arguments: transfer([moved]),
          },
        ),
      ),
    ).rejects.toThrow("data-only");
    expect(entered).toBe(0);
    expect(destroyed).toBe(1);
    const next = new ResourceOwner(nominal, {}, 1, {}, () => {
      destroyed++;
    });
    const record = host.session.handles.reference(reference);
    reference.release();
    await expect(
      host.root((context) =>
        host.session.calls.invokePhysical(record, "take", [0], context, {
          signature: "moves",
          input: encode([0]),
          arguments: transfer([next]),
        }),
      ),
    ).rejects.toThrow();
    expect(destroyed).toBe(2);
  } finally {
    moved.close();
    reference.release();
    await host.close();
  }
});

test("post-admission failure leaves moved ownership with its recipient until physical release", async () => {
  const host = createHost();
  const actual = Promise.withResolvers<void>();
  let destroyed = 0;
  const original = new ResourceOwner(nominal, {}, 1, {}, () => {
    destroyed++;
  });
  const moved = original.move();
  const batch = transfer([moved]);
  let end: (() => void) | undefined;
  const handle = bindCaptured(
    () => 0,
    "moves",
    async (_env, _ctx, _input, _receiver, retain, arguments_) => {
      expect(arguments_).toBe(batch);
      arguments_!.accept();
      end = moved.lend();
      retain!(actual.promise);
      throw new Error("bounded failure after ownership acceptance");
    },
  );
  await host.install({
    name: "moves",
    apiVersion: 1,
    services: { moves: { methods: { take: handle } } },
  });
  const reference = await host.get("moves", "main");
  try {
    await expect(
      host.root((context) =>
        host.session.calls.invokePhysical(
          host.session.handles.reference(reference),
          "take",
          [0],
          context,
          { signature: "moves", input: encode([0]), arguments: batch },
        ),
      ),
    ).rejects.toThrow("bounded failure");
    original.close();
    expect(destroyed).toBe(0);
    expect(host.diagnostics().calls).toBe(1);
    moved.close();
    expect(destroyed).toBe(0);
    actual.resolve();
    end!();
    await Promise.allSettled([...host.session.jobs]);
    expect(destroyed).toBe(1);
    expect(host.diagnostics().calls).toBe(0);
  } finally {
    actual.resolve();
    end?.();
    moved.close();
    reference.release();
    await host.close();
  }
});

test("foreign argument classes preserve the original owner and revoke sender aliases immediately", async () => {
  const callback = {
    ended: new AbortController(),
    scope: { releaseResource: (owner: ResourceOwner) => owner.close() },
  };
  const port = { current: () => callback } as unknown as ComponentPort;
  resourceImport(port, binding, [], [nominal]);
  const owner = new ResourceOwner(nominal, {}, 7, {}, () => {});
  const foreign = importedResource(port, nominal);
  const value = foreign.wrap(owner);
  const moved = foreign.move(value);
  expect(() => foreign.move(value)).toThrow("not live");
  expect(() => owner.issuedBy({})).toThrow("not live");
  const next = foreign.wrap(moved);
  (value as Disposable)[Symbol.dispose]();
  expect(moved.issuedBy({})).toBe(false);
  (next as Disposable)[Symbol.dispose]();
  expect(() => moved.issuedBy({})).toThrow("not live");
});

test("argument mapper requires imported nominal definitions for foreign aliases", () => {
  const fn: GraphFunction = {
    name: "accept",
    params: [{ name: "value", type: 1 }],
    result: "u64",
  };
  expect(
    resourceRoles(graph, binding, fn).arguments!(["owner"], (_nominal, value) => value),
  ).toEqual(["owner"]);
  expect(() => resourceRoles(graph, "test:moves/relay@1.0.0", fn)).toThrow("imported defining");
  const imported = { ...graph, imports: [{ binding }] };
  const roles = resourceRoles(imported, "test:moves/relay@1.0.0", fn);
  expect(
    roles.arguments!([0], (type, value) => {
      expect(type).toEqual(nominal);
      return value;
    }),
  ).toEqual([0]);
});

const callbackScope = (): CallbackScope => ({
  check() {},
  cancel() {},
  report() {},
  releaseResource(owner) {
    owner.close();
  },
  release() {},
  call: async () => null,
  acquire: async () => {
    throw new Error("unused");
  },
  invoke: async () => {
    throw new Error("unused");
  },
});

test("prepared argument preflight checks the complete batch before ACK or guest entry", async () => {
  const fn: GraphFunction = {
    name: "take",
    params: [{ name: "values", type: 2 }],
    result: "u64",
  };
  const types: TypeGraph = {
    ...graph,
    types: [...graph.types, { id: 2, definition: { kind: { list: 1 } } }],
  };
  const roles = resourceRoles(types, binding, fn);
  const port = new ComponentPort(100, async () => {});
  let entered = 0;
  port.exports = {
    [binding]: {
      take: () => {
        entered++;
        return 0n;
      },
    },
  };
  const revision = new ComponentRevision(
    port,
    {
      validate: () => '{"ok":null}',
      bind: () => 0,
      request: () => "{}",
    },
    { ...types, imports: [], exports: [], references: {} } as Described,
    100,
  );
  const environment = { id: 8 } as ComponentScope;
  revision.scopes.set(8, environment);
  for (const variant of ["duplicate", "duplicate-owner", "unused", "nominal", "missing"] as const) {
    const first = new ResourceOwner(nominal, revision, 7, {}, () => {});
    const second = new ResourceOwner(
      variant === "nominal" ? { ...nominal, name: "other" } : nominal,
      revision,
      7,
      {},
      () => {},
    );
    const batch = transfer(variant === "duplicate-owner" ? [first, first] : [first, second]);
    const indices =
      variant === "duplicate"
        ? [0, 0]
        : variant === "unused"
          ? [0]
          : variant === "missing"
            ? [0, 2]
            : [0, 1];
    await expect(
      revision["invoke"](
        binding,
        "take",
        converters(types)(resourceDataFunction(fn, roles)),
        0,
        { params: undefined, result: "u64" },
        environment,
        callbackScope(),
        encode([indices]),
        roles,
        undefined,
        undefined,
        batch,
      ),
    ).rejects.toThrow();
    expect(batch.accepted).toBe(false);
    first.close();
    second.close();
  }
  expect(entered).toBe(0);
  expect(port.usable).toBe(true);
  await port.close();
});

test("same-issuer owned arguments return the original backend across keyed environments", async () => {
  const fn: GraphFunction = {
    name: "take",
    params: [{ name: "value", type: 1 }],
    result: "u64",
  };
  const roles = resourceRoles(graph, binding, fn);
  const port = new ComponentPort(100, async () => {});
  let destroyed = 0;
  const raw = {
    [Symbol.dispose]() {
      destroyed++;
    },
  };
  port.exports = {
    [binding]: {
      take: (value) => {
        port.observer.taskStarted({ id: 1, entry: "take", isAsync: false });
        expect(value).toBe(raw);
        raw[Symbol.dispose]();
        queueMicrotask(() => port.observer.taskExited({ id: 1 }));
        return 19n;
      },
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
    100,
  );
  const environment = { id: 8 } as ComponentScope;
  revision.scopes.set(8, environment);
  const original = new ResourceOwner(nominal, revision, 7, raw, () => raw[Symbol.dispose]());
  const moved = original.move();
  const batch = transfer([moved]);
  const result = await revision["invoke"](
    binding,
    "take",
    converters(graph)(resourceDataFunction(fn, roles)),
    0,
    { params: undefined, result: "u64" },
    environment,
    callbackScope(),
    encode([0]),
    roles,
    undefined,
    undefined,
    batch,
  );
  expect(batch.accepted).toBe(true);
  expect("text" in result && result.text).toBe('"19"');
  original.close();
  moved.close();
  expect(destroyed).toBe(1);
  await port.close();
});
