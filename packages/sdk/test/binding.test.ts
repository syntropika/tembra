import { test } from "bun:test";
import assert from "node:assert/strict";

import {
  Type,
  bindReference,
  hostNamed,
  local,
  method,
  native,
  opaque,
  remote,
} from "../src/index.js";
import type { BindingPort, CallRequest, ContextHandle, Payload } from "../src/index.js";

const Counter = remote("binding.counter", {
  version: 1,
  input: Type.Object({ name: Type.String() }),
  host: hostNamed("service"),
  variants: {
    viewer: {
      state: { count: Type.Integer() },
      methods: { read: method(undefined, Type.Integer()) },
    },
    editor: {
      state: { count: Type.Integer() },
      methods: {
        increment: method(Type.Object({ by: Type.Integer() }), Type.Integer()),
        touch: method(),
        nullable: method(Type.Null(), Type.Null()),
        read: method(undefined, Type.Integer()),
      },
    },
  },
});

function harness() {
  let count = 1;
  const requests: CallRequest[] = [];
  const port: BindingPort = {
    status: "live",
    variant: "editor",
    shape: undefined,
    error: undefined,
    ready: Promise.resolve(),
    async call(request): Promise<Payload> {
      requests.push(request);
      if (request.member === "increment") {
        assert.equal(request.args.present, true);
        if (request.args.present) {
          const { value } = request.args;
          assert.ok(
            value !== null &&
              typeof value === "object" &&
              "by" in value &&
              typeof value.by === "number",
          );
          count += value.by;
        }
      }
      if (request.member === "touch") {
        return { present: false };
      }
      return {
        present: true,
        value: request.member === "nullable" ? null : count,
      };
    },
    readable() {
      return {
        get value() {
          return count;
        },
        subscribe(listener) {
          listener(count);
          return () => {
            /* Fixture callback has no runtime effects. */
          };
        },
      };
    },
    opaque() {
      throw new Error("no opaque members");
    },
    release() {
      /* Fixture callback has no runtime effects. */
    },
    onStatus() {
      return () => {
        /* Fixture callback has no runtime effects. */
      };
    },
    onShape() {
      return () => {
        /* Fixture callback has no runtime effects. */
      };
    },
  };
  return { port, requests };
}

test("binding wrappers invoke an injected Rust-adapter port with explicit absence/null and readable handles", async () => {
  const { port, requests } = harness();
  const reference = bindReference(Counter, port);
  assert.equal(reference.variant, "editor");
  if (reference.variant !== "editor") {
    throw new Error("wrong fixture variant");
  }
  assert.equal(reference.count.value, 1);
  assert.equal(await reference.increment({ by: 2 }), 3);
  assert.equal(reference.count.value, 3);
  assert.deepEqual(requests[0]?.args, { present: true, value: { by: 2 } });
  assert.equal(await reference.read(), 3);
  assert.deepEqual(requests[1]?.args, { present: false });
  const touchCall: unknown = Reflect.apply(reference.touch, reference, []);
  const touched = await touchCall;
  assert.equal(touched, undefined);
  assert.equal(await reference.nullable(null), null);
  assert.deepEqual(requests[3]?.args, { present: true, value: null });
  assert.equal(await Promise.resolve(reference), reference);
});

async function invalidListener(): Promise<void> {
  /* An asynchronous callback must reach the backend's synchronous refusal guard. */
}

test("readable wrappers preserve callback results for the synchronous backend guard", () => {
  const { port } = harness();
  const reference = bindReference(Counter, {
    ...port,
    readable() {
      return {
        value: 1,
        subscribe(listener) {
          const callback: unknown = listener;
          assert.ok(typeof callback === "function");
          const completion: unknown = Reflect.apply(callback, undefined, [1]);
          if (completion instanceof Promise) {
            throw new TypeError("async listener refused");
          }
          return () => {
            /* The fixture has no retained subscription. */
          };
        },
      };
    },
  });
  if (reference.variant !== "editor") {
    throw new Error("wrong fixture variant");
  }
  const subscribe: unknown = Reflect.get(reference.count, "subscribe");
  assert.ok(typeof subscribe === "function");
  assert.throws(() => {
    Reflect.apply(subscribe, reference.count, [invalidListener]);
  }, /async listener refused/u);
});

test("binding validation rejects malformed dynamic values without invoking the port", async () => {
  const { port, requests } = harness();
  const reference = bindReference(Counter, port);
  if (reference.variant !== "editor") {
    throw new Error("wrong fixture variant");
  }
  const dynamic = async (input: unknown): Promise<unknown> => {
    const result: unknown = Reflect.apply(reference.increment, reference, [input]);
    return await Promise.resolve(result);
  };
  await assert.rejects(dynamic({ by: "wrong" }), /validation/u);
  assert.equal(requests.length, 0);
  assert.equal(await dynamic({ by: 2, unknown: true }), 3);
  assert.deepEqual(requests[0]?.args, { present: true, value: { by: 2 } });
  const invalidPort = {
    ...port,
    async call(): Promise<Payload> {
      return { present: true, value: "wrong" };
    },
  };
  const bad = bindReference(Counter, invalidPort);
  if (bad.variant === "editor") {
    await assert.rejects(bad.read(), /validation/u);
  }
  const invalidState = bindReference(Counter, {
    ...port,
    readable() {
      return {
        value: "wrong",
        subscribe(listener) {
          listener("wrong");
          return () => {
            /* Fixture callback has no runtime effects. */
          };
        },
      };
    },
  });
  if (invalidState.variant === "editor") {
    assert.throws(() => invalidState.count.value, /validation/u);
    assert.throws(
      () =>
        invalidState.count.subscribe(() => {
          /* Fixture callback has no runtime effects. */
        }),
      /validation/u,
    );
  }
});

test("native local capabilities retain identity through an explicit binding port", async () => {
  const Native = local("binding.native", {
    version: 1,
    input: Type.Object({}),
    members: {
      echo: method(native<URL>("url@1"), native<URL>("url@1")),
      issue: method(undefined, native<URL>("url@1")),
      discard: method(native<URL>("url@1")),
      capability: opaque<URL>("url@1"),
    },
  });
  const { port } = harness();
  const context: ContextHandle = { runtime: "tembra", handle: {} };
  let entries = 0;
  const value = new URL("https://example.com/");
  const reference = bindReference(Native, {
    ...port,
    variant: undefined,
    opaque() {
      return value;
    },
    async call(request) {
      entries += 1;
      assert.equal(request.context, context);
      assert.equal(request.args.present, true);
      return request.args;
    },
  });
  await assert.rejects(
    async () => {
      await Reflect.apply(reference.echo, reference, [value]);
    },
    (error) => {
      assert.ok(error instanceof Error);
      assert.equal(Reflect.get(error, "code"), "invalid");
      assert.equal(Reflect.get(error, "reason"), "native_context");
      assert.equal(Reflect.get(error, "detailsJson"), "null");
      const serialized: unknown = Reflect.get(error, "tembraRuntimeError");
      assert.equal(typeof serialized, "string");
      if (typeof serialized !== "string") {
        throw new TypeError("Expected structured runtime metadata");
      }
      assert.deepEqual(JSON.parse(serialized), {
        code: "invalid",
        reason: "native_context",
        message: error.message,
        details: null,
      });
      assert.deepEqual(Reflect.get(error, Symbol.for("errorAdditionalPropertyKeys")), [
        "tembraRuntimeError",
      ]);
      return true;
    },
  );
  assert.equal(entries, 0);
  const result: URL = await reference.echo(value, context);
  assert.equal(result, value);
  assert.equal(reference.capability, value);
  async function typeChecks() {
    // @ts-expect-error: native nominal method does not accept a string
    await reference.echo("wrong", context);
    // @ts-expect-error: native arguments require an explicit caller scope
    await reference.echo(value);
    // @ts-expect-error: native results require scope even without arguments
    await reference.issue();
    // @ts-expect-error: native arguments require scope even with absent result
    await reference.discard(value);
    const issued: Promise<URL> = reference.issue(context);
    const discarded: Promise<undefined> = reference.discard(value, context);
    await Promise.all([issued, discarded]);
  }
  void typeChecks;
});

// These bodies typecheck but never execute; dynamic inputs are checked separately above.
async function typeExpectations(port: BindingPort) {
  const reference = bindReference(Counter, port);
  if (reference.variant === "viewer") {
    const count: number = reference.count.value;
    void count;
    // @ts-expect-error: viewer variant does not expose increment
    void reference.increment;
  }
  if (reference.variant === "editor") {
    const result: Promise<number> = reference.increment({ by: 1 });
    await result;
    // @ts-expect-error: by is numeric
    await reference.increment({ by: "wrong" });
    // @ts-expect-error: required arguments cannot be omitted
    await reference.increment();
    // @ts-expect-error: no-argument methods do not accept a JSON argument
    await reference.read(4);
    // @ts-expect-error: nullable method still needs its explicit null argument
    await reference.nullable();
  }
}
void typeExpectations;
