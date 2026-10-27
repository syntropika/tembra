import { expect, test } from "bun:test";
import { jsonCodec } from "../src/codec.ts";
import { Channel, type Wire } from "../src/infrastructure/rpc/channel.ts";
import type { Transport } from "../src/rpc.ts";
import { RemoteHost } from "../src/rpc.ts";

function transport() {
  let receive = (_frame: Uint8Array) => {};
  let closed = (_cause?: unknown) => {};
  const causes: unknown[] = [];
  const notifications: unknown[] = [];
  const sent: Wire[] = [];
  let detaches = 0;
  const endpoint: Transport = {
    send(frame) {
      sent.push(jsonCodec.deserialize(frame) as Wire);
    },
    listen(onFrame, onClosed) {
      receive = onFrame;
      closed = onClosed;
      return () => {
        detaches++;
        receive = () => {};
        closed = () => {};
      };
    },
    close(cause) {
      expect(detaches).toBe(1);
      causes.push(cause);
      closed(cause);
    },
  };
  return {
    endpoint,
    causes,
    notifications,
    sent,
    receive: (value: unknown) => receive(jsonCodec.serialize(value)),
    disconnect: (cause?: unknown) => closed(cause),
    ended: (cause: unknown) => notifications.push(cause),
    detaches: () => detaches,
  };
}

test("malformed frames report their terminal cause after transport listeners detach", () => {
  const connection = transport();
  const errors: unknown[] = [];
  const channel = new Channel(
    connection.endpoint,
    { onError: (error) => errors.push(error) },
    () => {},
    connection.ended,
  );

  connection.receive([]);

  expect(channel.closed).toBe(true);
  expect(errors).toHaveLength(1);
  expect(errors[0]).toMatchObject({ code: "invalid", reason: "rpc_frame" });
  expect(connection.causes).toEqual(errors);
  expect(connection.notifications).toEqual(errors);
  channel.close(new Error("later close"));
  connection.disconnect(new Error("later disconnect"));
  connection.receive(null);
  expect(connection.causes).toEqual(errors);
  expect(connection.notifications).toEqual(errors);
  expect(connection.detaches()).toBe(1);
});

test("transport disconnect forwards the original cause once", () => {
  const connection = transport();
  const cause = new Error("connection lost");
  const channel = new Channel(connection.endpoint, {}, () => {}, connection.ended);

  connection.disconnect(cause);
  channel.close();

  expect(connection.causes).toEqual([cause]);
  expect(connection.notifications).toEqual([cause]);
  expect(connection.detaches()).toBe(1);
});

test("explicit close provides the default connection cause once", () => {
  const connection = transport();
  const channel = new Channel(connection.endpoint, {}, () => {}, connection.ended);

  channel.close();
  channel.close();

  expect(connection.causes).toHaveLength(1);
  expect(connection.causes[0]).toMatchObject({ code: "gone", reason: "connection" });
  expect(connection.notifications).toEqual(connection.causes);
  expect(connection.detaches()).toBe(1);
});

test("remote observer errors report without closing the transport", async () => {
  const connection = transport();
  const errors: unknown[] = [];
  const remote = new RemoteHost(connection.endpoint, { onError: (error) => errors.push(error) });
  const acquiring = remote.get("counter", "main");
  connection.receive({ id: 1, ok: { reference: 1, methods: [], state: { value: 0 } } });
  const reference = await acquiring;
  const cause = new Error("observer failed");
  const seen: number[] = [];
  reference.state.subscribe((value) => {
    if (value.value > 0) throw cause;
  });
  reference.state.subscribe((value) => seen.push(value.value));

  connection.receive({ event: "state", payload: { reference: 1, value: { value: 1 } } });
  connection.receive({ event: "state", payload: { reference: 1, value: { value: 2 } } });

  expect(errors).toEqual([cause, cause]);
  expect(seen).toEqual([0, 1, 2]);
  expect(reference.status).toBe("live");
  expect(connection.causes).toEqual([]);
  remote.close();
  expect(connection.causes).toHaveLength(1);
  expect(connection.causes[0]).toMatchObject({ code: "gone", reason: "connection" });
});

test("remote acquisition retains state events delivered immediately after its response", async () => {
  const connection = transport();
  const remote = new RemoteHost(connection.endpoint);
  const acquiring = remote.get("counter", "main");

  connection.receive({ id: 1, ok: { reference: 1, methods: [], state: { value: 0 } } });
  connection.receive({ event: "state", payload: { reference: 1, value: { value: 1 } } });
  connection.receive({ event: "state", payload: { reference: 1, value: { value: 2 } } });
  const reference = await acquiring;
  const seen: number[] = [];
  const detach = reference.state.subscribe((value) => seen.push(value.value));

  expect(reference.state.value).toEqual({ value: 2 });
  expect(seen).toEqual([2]);
  detach();
  const state = reference.state;
  reference.release();
  connection.receive({ event: "state", payload: { reference: 1, value: { value: 3 } } });
  expect(reference.status).toBe("released");
  expect(() => state.value).toThrow(expect.objectContaining({ code: "released" }));
  remote.close();
});

test("cancelled acquisition releases a late claim without attaching its state", async () => {
  const connection = transport();
  const remote = new RemoteHost(connection.endpoint);
  const controller = new AbortController();
  const cause = new Error("acquisition cancelled");
  const acquiring = remote.get("counter", "cancelled", { signal: controller.signal });
  controller.abort(cause);
  await expect(acquiring).rejects.toBe(cause);
  connection.receive({ id: 1, ok: { reference: 1, methods: [], state: { value: 0 } } });
  connection.receive({ event: "state", payload: { reference: 1, value: { value: 1 } } });
  expect(connection.sent).toEqual([
    { id: 1, op: "get", payload: { service: "counter", key: ["cancelled"] } },
    { op: "cancel", payload: { id: 1 } },
    { id: 2, op: "release", payload: { reference: 1 } },
  ]);
  connection.receive({ id: 2, ok: null });

  const next = remote.get("counter", "live");
  connection.receive({ id: 3, ok: { reference: 2, methods: [], state: { value: 2 } } });
  connection.receive({ event: "state", payload: { reference: 2, value: { value: 3 } } });
  const reference = await next;
  expect(reference.status).toBe("live");
  expect(reference.state.value).toEqual({ value: 3 });
  remote.close();
  expect(reference.status).toBe("gone");
});

test("throwing transport close and error reporter still revoke mirrors and pending work once", async () => {
  const connection = transport();
  const closeError = new Error("transport close failed");
  const reported: unknown[] = [];
  const close = connection.endpoint.close.bind(connection.endpoint);
  connection.endpoint.close = (cause) => {
    close(cause);
    throw closeError;
  };
  const remote = new RemoteHost(connection.endpoint, {
    onError(error) {
      reported.push(error);
      throw new Error("reporter failed");
    },
  });
  const acquiring = remote.get("counter", "main");
  connection.receive({ id: 1, ok: { reference: 1, methods: [], state: { value: 0 } } });
  const reference = await acquiring;
  const state = reference.state;
  const pending = remote.installedOrbs();
  const cause = new Error("connection lost");

  expect(() => remote.channel.close(cause)).not.toThrow();
  await expect(pending).rejects.toBe(cause);
  expect(reference.status).toBe("gone");
  expect(() => state.value).toThrow(expect.objectContaining({ code: "gone" }));
  remote.close();
  expect(connection.causes).toEqual([cause]);
  expect(connection.detaches()).toBe(1);
  expect(reported).toEqual([closeError]);
});

test("a throwing frame error reporter cannot prevent terminal cleanup", async () => {
  const connection = transport();
  const reported: unknown[] = [];
  const remote = new RemoteHost(connection.endpoint, {
    onError(error) {
      reported.push(error);
      throw new Error("reporter failed");
    },
  });
  const pending = remote.installedOrbs();

  expect(() => connection.receive([])).not.toThrow();
  await expect(pending).rejects.toBe(reported[0]);
  expect(connection.causes).toEqual(reported);
  expect(connection.detaches()).toBe(1);
});

test("throwing detachment and error reporter still close and reject pending work once", async () => {
  const connection = transport();
  const detachError = new Error("transport detach failed");
  const reported: unknown[] = [];
  const listen = connection.endpoint.listen.bind(connection.endpoint);
  connection.endpoint.listen = (receive, closed) => {
    const detach = listen(receive, closed);
    return () => {
      detach();
      throw detachError;
    };
  };
  const remote = new RemoteHost(connection.endpoint, {
    onError(error) {
      reported.push(error);
      throw new Error("reporter failed");
    },
  });
  const pending = remote.installedOrbs();
  const cause = new Error("connection lost");

  expect(() => remote.channel.close(cause)).not.toThrow();
  await expect(pending).rejects.toBe(cause);
  remote.close();
  expect(connection.causes).toEqual([cause]);
  expect(connection.detaches()).toBe(1);
  expect(reported).toEqual([detachError]);
});

test("aborted acquisitions and their cleanup retain a bounded pending budget", async () => {
  const connection = transport();
  const remote = new RemoteHost(connection.endpoint, { maxPending: 2 });
  const cancelled = [];
  for (let index = 0; index < 2; index++) {
    const controller = new AbortController();
    const acquiring = remote.get("counter", String(index), { signal: controller.signal });
    controller.abort();
    cancelled.push(expect(acquiring).rejects.toMatchObject({ name: "AbortError" }));
  }
  await Promise.all(cancelled);
  await expect(remote.get("counter", "blocked")).rejects.toMatchObject({
    code: "exhausted",
    reason: "pending_requests",
  });
  expect(connection.sent.filter((message) => message.op === "get")).toHaveLength(2);

  connection.receive({ id: 1, ok: { reference: 1, methods: [], state: { value: 0 } } });
  await expect(remote.get("counter", "still-blocked")).rejects.toMatchObject({
    code: "exhausted",
  });
  connection.receive({
    id: 2,
    error: { code: "cancelled", reason: "request", message: "Cancelled" },
  });
  const acquiring = remote.get("counter", "recovered");
  connection.receive({ id: 4, ok: { reference: 4, methods: [], state: { value: 4 } } });
  expect((await acquiring).state.value).toEqual({ value: 4 });
  connection.receive({ id: 3, ok: null });
  expect(connection.sent.filter((message) => message.op === "release")).toEqual([
    { id: 3, op: "release", payload: { reference: 1 } },
  ]);
  remote.close();
});

test("aborting after a response cannot install a pending tombstone", async () => {
  const connection = transport();
  const remote = new RemoteHost(connection.endpoint, { maxPending: 1 });
  const controller = new AbortController();
  const acquiring = remote.get("counter", "main", { signal: controller.signal });
  connection.receive({ id: 1, ok: { reference: 1, methods: [], state: { value: 0 } } });
  controller.abort();
  expect((await acquiring).state.value).toEqual({ value: 0 });

  const describing = remote.installedOrbs();
  connection.receive({ id: 2, ok: [] });
  await expect(describing).resolves.toEqual([]);
  expect(connection.sent.some((message) => message.op === "cancel")).toBe(false);
  remote.close();
});

test("an unusable acquisition releases only its valid server claim", async () => {
  const connection = transport();
  const remote = new RemoteHost(connection.endpoint, { maxPending: 1 });
  const acquiring = remote.get("counter", "invalid-methods");
  connection.receive({ id: 1, ok: { reference: 7, methods: null, state: { value: 0 } } });
  await expect(acquiring).rejects.toMatchObject({ code: "invalid", reason: "rpc_frame" });
  expect(connection.sent.at(-1)).toEqual({ id: 2, op: "release", payload: { reference: 7 } });
  connection.receive({ id: 2, ok: null });

  const malformed = remote.get("counter", "invalid-reference");
  connection.receive({ id: 3, ok: { reference: "untrusted", methods: [] } });
  await expect(malformed).rejects.toMatchObject({ code: "invalid", reason: "rpc_frame" });
  connection.receive({ id: 3, ok: { reference: 8, methods: [] } });
  expect(connection.sent.filter((message) => message.op === "release")).toHaveLength(1);
  remote.close();
});
