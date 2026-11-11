import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { bindCaptured, type CapturedInvocation } from "../src/runtime/adapters.ts";
import { ResourceOwner } from "../src/interpreter/component/resources.ts";
import { encode } from "../src/interpreter/codec.ts";
import { KernelOp } from "../src/bridge/protocol.ts";

const identity = { interface: "test:claims/objects@1.0.0", name: "object" };

test("duplicate resource claims retain exact authority and survive original root cleanup", async () => {
  const host = createHost({ principal: "alice" });
  const definition = (version: number) => ({
    name: "claims",
    apiVersion: 1,
    services: {
      objects: {
        access: () => ({ methods: ["read"] }),
        methods: {
          read: () => version,
          secret: () => "secret",
        },
      },
    },
  });
  await host.install(definition(1));
  let copy!: ReturnType<typeof host.session.handles.duplicate>;
  try {
    await host.root(async (context) => {
      const reference = await host.session.handles.get("objects", "main", {}, context);
      const original = host.session.handles.reference(reference);
      copy = host.session.handles.duplicate(original);
      expect(copy.id).not.toBe(original.id);
      expect(copy.object).toBe(original.object);
      expect(copy.address.principal).toBe("alice");
      expect(copy.methods).toEqual(new Set(["read"]));
      expect(copy.context).toBeUndefined();
      await expect(host.session.calls.invoke(copy, "secret", [], context)).rejects.toMatchObject({
        code: "forbidden",
      });
    });
    expect(copy.released).toBe(false);
    expect(await host.root((context) => host.session.calls.invoke(copy, "read", [], context))).toBe(
      1,
    );
    await host.reload([definition(2)]);
    expect(await host.root((context) => host.session.calls.invoke(copy, "read", [], context))).toBe(
      2,
    );
    expect(copy.methods).toEqual(new Set(["read"]));
    await expect(
      host.root((context) => host.session.calls.invoke(copy, "secret", [], context)),
    ).rejects.toMatchObject({ code: "forbidden" });
    host.session.handles.release(copy.id);
    expect(() => host.session.handles.duplicate(copy)).toThrow();
  } finally {
    if (copy) host.session.handles.release(copy.id);
    await host.close();
  }
});

test("an early capability head keeps its provider invocation admitted until physical completion", async () => {
  const host = createHost();
  const finished = Promise.withResolvers<void>();
  const origin = {};
  const resource = new ResourceOwner(identity, origin, 1, {}, () => {});
  const handle = bindCaptured(
    () => {
      throw new Error("data handler");
    },
    "resource-one",
    async (_environment, _context, _input, _receiver, retain) => {
      retain!(finished.promise);
      return {
        value: encode(0),
        capabilities: [resource],
        finished: finished.promise,
      };
    },
  );
  await host.install({
    name: "early",
    apiVersion: 1,
    services: { objects: { methods: { open: handle } } },
  });
  const reference = await host.get("objects", "main");
  const record = host.session.handles.reference(reference);
  try {
    await host.root(async (context) => {
      const capture: CapturedInvocation = {
        signature: "resource-one",
        input: encode([]),
      };
      await host.session.calls.invokePhysical(record, "open", [], context, capture);
      expect(capture.capabilities).toEqual([resource]);
      expect(host.diagnostics().calls).toBe(1);
      finished.resolve();
      await Promise.allSettled([...host.session.jobs]);
      expect(host.diagnostics().calls).toBe(0);
      resource.close();
    });
  } finally {
    finished.resolve();
    resource.close();
    reference.release();
    await host.close();
  }
});

test("reload never forwards an old implicit resource receiver to an ordinary handler", async () => {
  const host = createHost();
  let dataCalls = 0;
  const resource = new ResourceOwner(identity, {}, 1, {}, () => {});
  const handle = bindCaptured(
    () => 1,
    "resource-one",
    async () => encode(1),
  );
  const definition = (read: typeof handle) => ({
    name: "receiver",
    apiVersion: 1,
    services: { objects: { methods: { read } } },
  });
  await host.install(definition(handle));
  const reference = await host.get("objects", "main");
  const record = host.session.handles.reference(reference);
  try {
    resource.anchor(host.session, record);
    await host.reload([
      definition(() => {
        dataCalls++;
        return 2;
      }),
    ]);
    await expect(
      host.root((context) =>
        host.session.calls.invokePhysical(resource.reference, "read", [], context, {
          signature: "resource-one",
          input: encode([]),
          receiver: resource,
        }),
      ),
    ).rejects.toThrow("data-only");
    expect(dataCalls).toBe(0);
    expect(host.session.kernel.command(KernelOp.Diagnostics).calls).toBe(0);
  } finally {
    resource.close();
    reference.release();
    await host.close();
  }
});

test("a bounded receiver failure retains its loan and invocation until actual provider exit", async () => {
  const host = createHost();
  const actual = Promise.withResolvers<void>();
  let destroyed = 0;
  const owner = new ResourceOwner(identity, {}, 1, {}, () => {
    destroyed++;
  });
  const method = bindCaptured(
    () => 0,
    "borrowed-timeout",
    async (_environment, _context, _input, receiver, retain) => {
      expect(receiver).toBeDefined();
      expect(receiver).not.toBe(owner);
      retain!(actual.promise);
      throw new Error("bounded callback timeout");
    },
  );
  await host.install({
    name: "borrowed-timeout",
    apiVersion: 1,
    services: { object: { methods: { read: method } } },
  });
  const reference = await host.get("object", "main");
  owner.anchor(host.session, host.session.handles.reference(reference));
  const retained = owner.reference;
  try {
    await host.root(async (context) => {
      await expect(
        host.session.calls.invokePhysical(retained, "read", [], context, {
          signature: "borrowed-timeout",
          input: encode([]),
          receiver: owner,
        }),
      ).rejects.toThrow("bounded callback timeout");
      owner.close();
      expect(destroyed).toBe(0);
      expect(retained.released).toBe(false);
      expect(host.diagnostics().calls).toBe(1);
    });
    expect(destroyed).toBe(0);
    expect(host.diagnostics().calls).toBe(1);
    reference.release();
    let closed = false;
    const closing = host.close().then(() => {
      closed = true;
    });
    await Promise.resolve();
    expect(closed).toBe(false);
    actual.resolve();
    await closing;
    expect(destroyed).toBe(1);
    expect(host.diagnostics().calls).toBe(0);
  } finally {
    actual.resolve();
    owner.close();
    reference.release();
    await host.close();
  }
});
