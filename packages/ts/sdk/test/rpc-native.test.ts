import { expect, test } from "bun:test";
import { jsonCodec } from "../src/codec.ts";
import { RemoteHost } from "../src/rpc.ts";
import type { Transport } from "../src/rpc.ts";

/** Carry unmodified RPC bytes over the example's newline-framed standard streams. */
function connect(executable: string) {
  const child = Bun.spawn([executable], { stdin: "pipe", stdout: "pipe", stderr: "pipe" });
  const stderr = new Response(child.stderr).text();
  const sent: any[] = [];
  const received: any[] = [];
  let receive = (_frame: Uint8Array) => {};
  let ended = (_cause?: unknown) => {};
  let closed = false;
  const transport: Transport = {
    async send(frame) {
      sent.push(jsonCodec.deserialize(frame));
      child.stdin.write(frame);
      child.stdin.write("\n");
      await child.stdin.flush();
    },
    listen(onFrame, onClosed) {
      receive = onFrame;
      ended = onClosed;
      return () => {
        receive = () => {};
        ended = () => {};
      };
    },
    close() {
      if (closed) return;
      closed = true;
      child.stdin.end();
    },
  };
  const output = (async () => {
    const reader = child.stdout.getReader();
    let pending = new Uint8Array(0);
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        const combined = new Uint8Array(pending.length + chunk.value.length);
        combined.set(pending);
        combined.set(chunk.value, pending.length);
        let start = 0;
        for (let index = 0; index < combined.length; index++) {
          if (combined[index] !== 10) continue;
          const frame = combined.slice(start, index);
          received.push(jsonCodec.deserialize(frame));
          receive(frame);
          start = index + 1;
        }
        pending = combined.slice(start);
      }
      expect(pending.length).toBe(0);
      ended();
    } catch (error) {
      ended(error);
      throw error;
    } finally {
      reader.releaseLock();
    }
  })();
  return { child, stderr, sent, received, output, remote: new RemoteHost(transport) };
}

/** Fail promptly and clean up the child when a terminal transport path stalls. */
async function terminalExit(child: Bun.Subprocess<"pipe", "pipe", "pipe">) {
  const status = await Promise.race([child.exited, Bun.sleep(2000).then(() => undefined)]);
  if (status === undefined) {
    child.kill();
    await child.exited;
    throw new Error("native RPC did not terminate while its stdin remained open");
  }
  expect(status).not.toBe(0);
}

test.skipIf(!process.env.TEMBRA_NATIVE_RPC)(
  "clean input EOF drains accepted replies before the process exits",
  async () => {
    const child = Bun.spawn([process.env.TEMBRA_NATIVE_RPC!], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    const stderr = new Response(child.stderr).text();
    const reader = child.stdout.getReader();
    const decoder = new TextDecoder();
    let output = "";
    try {
      child.stdin.write(
        jsonCodec.serialize({
          id: 1,
          op: "get",
          payload: { service: "example.counter", key: ["eof-drain"] },
        }),
      );
      child.stdin.write("\n");
      await child.stdin.flush();
      while (!output.includes("\n")) {
        const chunk = await reader.read();
        expect(chunk.done).toBe(false);
        output += decoder.decode(chunk.value);
      }
      const boundary = output.indexOf("\n");
      const acquired = JSON.parse(output.slice(0, boundary)).ok;
      output = output.slice(boundary + 1);
      const payload = "x".repeat(48 * 1024);
      // Four replies fit both application limits, while unread pipe/stream buffers can fill.
      for (const id of [2, 3, 4, 5]) {
        child.stdin.write(
          jsonCodec.serialize({
            id,
            op: "call",
            payload: { reference: acquired.reference, method: "echo", arguments: [payload] },
          }),
        );
        child.stdin.write("\n");
      }
      await child.stdin.flush();
      await Bun.sleep(50);
      await child.stdin.end();
      // Keep output unread across EOF so accepted data must survive writer shutdown.
      await Bun.sleep(25);
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        output += decoder.decode(chunk.value);
      }
      expect(output.endsWith("\n")).toBe(true);
      expect(
        output
          .trimEnd()
          .split("\n")
          .map((line) => JSON.parse(line)),
      ).toEqual([
        { id: 2, ok: payload },
        { id: 3, ok: payload },
        { id: 4, ok: payload },
        { id: 5, ok: payload },
      ]);
      expect(await child.exited).toBe(0);
      expect(await stderr).toBe("");
    } finally {
      reader.releaseLock();
      if (child.exitCode === null) {
        child.kill();
        await child.exited;
      }
    }
  },
);

test.skipIf(!process.env.TEMBRA_NATIVE_RPC)(
  "native writer failure terminates with its cause while stdin stays open",
  async () => {
    const child = Bun.spawn([process.env.TEMBRA_NATIVE_RPC!], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    const stderr = new Response(child.stderr).text();
    await child.stdout.cancel();
    child.stdin.write(jsonCodec.serialize({ id: 1, op: "describe", payload: null }));
    child.stdin.write("\n");
    await child.stdin.flush();
    await terminalExit(child);
    expect(await stderr).toContain('reason: "stdio"');
  },
);

test.skipIf(!process.env.TEMBRA_NATIVE_RPC)(
  "unread output exhausts the bounded writer without blocking the host executor",
  async () => {
    const child = Bun.spawn([process.env.TEMBRA_NATIVE_RPC!], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    const stderr = new Response(child.stderr).text();
    const reader = child.stdout.getReader();
    child.stdin.write(
      jsonCodec.serialize({
        id: 1,
        op: "get",
        payload: {
          service: "example.counter",
          key: ["unread-output"],
        },
      }),
    );
    child.stdin.write("\n");
    await child.stdin.flush();
    let acquired = "";
    while (!acquired.includes("\n")) {
      const chunk = await reader.read();
      if (chunk.done) throw new Error("native provider ended before acquisition");
      acquired += new TextDecoder().decode(chunk.value);
    }
    const reference = JSON.parse(acquired.trim()).ok.reference;
    // Large replies exceed unread stream/pipe storage while each request remains within limits.
    const frames = Array.from({ length: 16 }, (_, index) =>
      jsonCodec.serialize({
        id: index + 2,
        op: "call",
        payload: {
          reference,
          method: "echo",
          arguments: ["x".repeat(48 * 1024)],
        },
      }),
    );
    const input = new Uint8Array(frames.reduce((bytes, frame) => bytes + frame.length + 1, 0));
    let offset = 0;
    for (const frame of frames) {
      input.set(frame, offset);
      offset += frame.length;
      input[offset++] = 10;
    }
    // The peer may close its read end during either the write or its asynchronous flush.
    try {
      child.stdin.write(input);
      await child.stdin.flush();
    } catch (error) {
      if (!(error && typeof error === "object" && "code" in error && error.code === "EPIPE"))
        throw error;
    }
    await terminalExit(child);
    expect(await stderr).toContain('reason: "output_queue"');
    reader.releaseLock();
    await child.stdout.cancel();
  },
);

test.skipIf(!process.env.TEMBRA_NATIVE_RPC)(
  "live oversized state terminates with its retained frame cause",
  async () => {
    const connection = connect(process.env.TEMBRA_NATIVE_RPC!);
    try {
      const reference = await connection.remote.get("example.counter", "oversized");
      await expect(reference.oversize()).rejects.toBeDefined();
      await terminalExit(connection.child);
      expect(await connection.stderr).toContain('reason: "frame_size"');
      await connection.output;
    } finally {
      connection.remote.close();
      if (connection.child.exitCode === null) {
        connection.child.kill();
        await connection.child.exited;
      }
    }
  },
);

test.skipIf(!process.env.TEMBRA_NATIVE_RPC)(
  "SDK RemoteHost exchanges describe/get/call/state/release/cancel bytes with native Rust",
  async () => {
    const connection = connect(process.env.TEMBRA_NATIVE_RPC!);
    const { remote } = connection;
    try {
      const catalog = await remote.installedOrbs();
      expect(catalog).toMatchObject([{ name: "rpc-example", apiVersion: 1 }]);
      await expect(remote.get("example.counter", "denied")).rejects.toMatchObject({
        code: "forbidden",
        reason: "service",
      });
      await expect(remote.get("unknown", "main")).rejects.toMatchObject({
        code: "forbidden",
        reason: "service",
      });
      const reference = await remote.get("example.counter", "main");
      const states: unknown[] = [];
      const detach = reference.state.subscribe((value) => states.push(value));
      expect(await reference.set(42)).toBe(42);
      expect(await reference.read()).toBe(42);
      expect(reference.state.value).toEqual({ value: 42, started: false });
      const initialStates = states.slice();
      expect(Object.isFrozen(reference.state.value)).toBe(true);

      const portable = {
        integer: -(1n << 100n),
        bytes: new Uint8Array([0, 10, 127, 128, 255]),
        text: "a frame can contain\na line break and Unicode: λ",
        taggedObject: { $tembra: ["bytes", [42]] },
      };
      expect(await reference.echo(portable)).toEqual(portable);
      const acquired = await remote.request("get", {
        service: "example.counter",
        key: ["main"],
        principal: "forged-client",
        methods: ["hidden"],
      });
      expect(acquired.methods).not.toContain("hidden");
      expect(reference.hidden).toBeUndefined();
      expect(
        await remote.request("call", {
          reference: acquired.reference,
          method: "principal",
          arguments: [],
          principal: "forged-client",
        }),
      ).toBe("trusted-client");
      for (const method of ["hidden", "constructor", "opaque"])
        await expect(
          remote.request("call", {
            reference: acquired.reference,
            method,
            arguments: [],
          }),
        ).rejects.toMatchObject({ code: "forbidden", reason: "method" });

      const waiting = new Promise<void>((resolve) => {
        const detachWaiting = reference.state.subscribe((value) => {
          if (!value.started) return;
          detachWaiting();
          resolve();
        });
      });
      const controller = new AbortController();
      const cancelled = new Error("cancel native call");
      const call = remote.request(
        "call",
        { reference: acquired.reference, method: "wait", arguments: [] },
        controller.signal,
      );
      const outcome = call.then(
        () => undefined,
        (error: unknown) => error,
      );
      await waiting;
      controller.abort(cancelled);
      expect(await outcome).toBe(cancelled);
      expect(await reference.cancelled()).toBe(1);
      expect(connection.sent.some((frame) => frame.op === "cancel")).toBe(true);

      expect(await remote.request("release", { reference: acquired.reference })).toBeNull();
      expect(await remote.request("release", { reference: acquired.reference })).toBeNull();
      await expect(
        remote.request("call", {
          reference: acquired.reference,
          method: "read",
          arguments: [],
        }),
      ).rejects.toMatchObject({ code: "forbidden", reason: "method" });
      detach();
      reference.release();
      reference.release();
      expect(reference.status).toBe("released");
      await expect(reference.read()).rejects.toMatchObject({ code: "released" });
      // A response after the release notification proves the server processed both in order.
      await remote.installedOrbs();
      const held = await remote.get("example.counter", "held-until-close");
      remote.close();
      expect(held.status).toBe("gone");
      await expect(held.read()).rejects.toMatchObject({ code: "gone" });
      await connection.output;
      expect(await connection.child.exited).toBe(0);
      expect(await connection.stderr).toBe("");
      expect(connection.received.some((frame) => frame.event === "state")).toBe(true);
      expect(initialStates).toEqual([
        { value: 0, started: false },
        { value: 42, started: false },
      ]);
    } finally {
      remote.close();
      const exited = await Promise.race([
        Promise.all([connection.child.exited, connection.output]).then(([code]) => code),
        Bun.sleep(1000).then(() => undefined),
      ]);
      if (exited === undefined) {
        connection.child.kill();
        await connection.child.exited;
        await connection.output;
      }
    }
  },
  15_000,
);
