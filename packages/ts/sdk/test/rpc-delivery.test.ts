import { expect, test } from "bun:test";
import { jsonCodec } from "../src/codec.ts";
import { RemoteHost, type Transport } from "../src/rpc.ts";
import type { Wire } from "../src/infrastructure/rpc/channel.ts";
import { TembraError } from "../src/error.ts";
import { Errors } from "../src/errors.ts";

function connection() {
  let receive = (_frame: Uint8Array) => {};
  const sent: Wire[] = [];
  const closed: unknown[] = [];
  const endpoint: Transport = {
    send(frame) {
      sent.push(jsonCodec.deserialize(frame) as Wire);
    },
    listen(onFrame) {
      receive = onFrame;
      return () => {
        receive = () => {};
      };
    },
    close(cause) {
      closed.push(cause);
    },
  };
  return {
    endpoint,
    sent,
    closed,
    reply(id: number, reference: number) {
      receive(jsonCodec.serialize({ id, ok: { reference, methods: [], state: { value: 7 } } }));
    },
  };
}

test("an oversized Get never reaches transport or closes the channel and frees its pending slot", async () => {
  const wire = connection();
  const remote = new RemoteHost(wire.endpoint, { maxFrame: 256, maxPending: 1 });
  try {
    await expect(remote.get("fixture.reader", "x".repeat(2048))).rejects.toMatchObject({
      code: "exhausted",
      reason: "frame_size",
    });
    expect(wire.sent).toHaveLength(0);
    expect(wire.closed).toHaveLength(0);
    const valid = remote.get("fixture.reader", "main");
    wire.reply(2, 1);
    expect((await valid).state.value).toEqual({ value: 7 });
    expect(wire.sent).toHaveLength(1);
    expect(wire.closed).toHaveLength(0);
  } finally {
    remote.close();
  }
});

test("a serialization failure preserves existing references and frees its pending slot", async () => {
  const wire = connection();
  const remote = new RemoteHost(wire.endpoint, { maxPending: 1 });
  try {
    const first = remote.get("fixture.reader", "main");
    wire.reply(1, 1);
    const reference = await first;
    await expect(
      remote.get("fixture.reader", [undefined] as unknown as string[]),
    ).rejects.toMatchObject({ code: "invalid", reason: "json_value" });
    expect(wire.sent).toHaveLength(1);
    expect(wire.closed).toHaveLength(0);
    expect(reference.state.value).toEqual({ value: 7 });
    const next = remote.get("fixture.reader", "next");
    wire.reply(3, 2);
    expect((await next).status).toBe("live");
  } finally {
    remote.close();
  }
});

for (const asynchronous of [false, true]) {
  for (const cancelled of [false, true]) {
    test(`a ${asynchronous ? "rejected" : "throwing"} transport closes uncertain Get delivery${cancelled ? " after cancellation" : ""}`, async () => {
      const wire = connection();
      const remote = new RemoteHost(wire.endpoint, { maxPending: 2 });
      const controller = new AbortController();
      const cancelledCause = new Error("cancelled acquisition");
      // The transport can use exactly the same code as a local size refusal.
      const cause = new TembraError(Errors.Exhausted.FrameSize);
      const first = remote.get("fixture.reader", "main");
      wire.reply(1, 1);
      const reference = await first;
      const unrelated = remote.installedOrbs().catch((error: unknown) => error);
      const send = wire.endpoint.send;
      wire.endpoint.send = (frame) => {
        send(frame);
        if ((jsonCodec.deserialize(frame) as Wire).op !== "get") return;
        if (cancelled) controller.abort(cancelledCause);
        if (asynchronous)
          return Promise.resolve().then(() => {
            throw cause;
          });
        throw cause;
      };
      const failed = remote.get("fixture.reader", "uncertain", { signal: controller.signal });
      await expect(failed).rejects.toBe(cancelled ? cancelledCause : cause);
      expect(await unrelated).toBe(cause);
      expect(wire.closed).toEqual([cause]);
      expect(reference.status).toBe("gone");
      expect(wire.sent.filter((frame) => frame.op === "get")).toHaveLength(2);
      await expect(remote.get("fixture.reader", "after")).rejects.toMatchObject({ code: "gone" });
      remote.close();
      expect(wire.closed).toEqual([cause]);
    });
  }
}

for (const duringEncoding of [false, true]) {
  test(`cancelling ${duringEncoding ? "during encoding" : "before local failure settles"} retains no pending acquisition or cleanup`, async () => {
    const wire = connection();
    const controller = new AbortController();
    const cause = new Error("cancelled locally");
    const remote = new RemoteHost(wire.endpoint, {
      maxPending: 1,
      maxFrame: 256,
      codec: {
        ...jsonCodec,
        serialize(value) {
          if (duringEncoding && (value as Wire).payload?.key?.[0] === "cancel")
            controller.abort(cause);
          return jsonCodec.serialize(value);
        },
      },
    });
    try {
      const failed = remote.get("fixture.reader", duringEncoding ? "cancel" : "x".repeat(2048), {
        signal: controller.signal,
      });
      controller.abort(cause);
      await expect(failed).rejects.toBe(cause);
      expect(wire.sent).toHaveLength(0);
      expect(wire.closed).toHaveLength(0);
      // Repeated acquisitions with one slot expose retained cancellation tombstones.
      for (let id = 2; id <= 4; id++) {
        const next = remote.get("fixture.reader", "valid");
        wire.reply(id, id);
        expect((await next).status).toBe("live");
      }
      expect(wire.sent.every((frame) => frame.op === "get")).toBe(true);
    } finally {
      remote.close();
    }
  });
}
