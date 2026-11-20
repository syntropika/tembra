/** Real Rust RPC frames travel over bounded newline-framed standard streams. */
import { expect } from "bun:test";
import { jsonCodec } from "../../src/codec.ts";
import { RemoteHost, type Transport } from "../../src/rpc.ts";

export interface Frame {
  id?: number;
  op?: string;
  payload?: Record<string, unknown>;
  ok?: unknown;
  error?: { code: string; reason: string };
}

/** Connect a generated SDK client to one actual native Rust host process. */
export function connect(
  mode: string,
  limits: { maxReferences?: number; maxFrame?: number; maxPending?: number } = {},
) {
  const child = Bun.spawn([process.env.TEMBRA_RPC_HOST!, mode], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...process.env,
      ...(limits.maxReferences ? { TEMBRA_RPC_MAX_REFERENCES: String(limits.maxReferences) } : {}),
      ...(limits.maxFrame ? { TEMBRA_RPC_MAX_FRAME: String(limits.maxFrame) } : {}),
    },
  });
  const stderr = new Response(child.stderr).text();
  const sent: Frame[] = [];
  const received: Frame[] = [];
  const held: Uint8Array[] = [];
  let filter: ((frame: Frame) => boolean) | undefined;
  let receive = (_frame: Uint8Array) => {};
  let ended = (_error?: unknown) => {};
  let closed = false;
  let sending: ((frame: Frame) => Promise<void> | undefined) | undefined;
  const transport: Transport = {
    async send(bytes) {
      const frame = jsonCodec.deserialize(bytes) as Frame;
      sent.push(frame);
      child.stdin.write(bytes);
      child.stdin.write("\n");
      await child.stdin.flush();
      await sending?.(frame);
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
      if (!closed) {
        closed = true;
        child.stdin.end();
      }
    },
  };
  const output = (async () => {
    const reader = child.stdout.getReader();
    let pending = new Uint8Array();
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) break;
        const bytes = new Uint8Array(pending.length + chunk.value.length);
        bytes.set(pending);
        bytes.set(chunk.value, pending.length);
        let start = 0;
        for (let index = 0; index < bytes.length; index++) {
          if (bytes[index] !== 10) continue;
          const frame = bytes.slice(start, index);
          const value = jsonCodec.deserialize(frame) as Frame;
          received.push(value);
          if (filter?.(value)) held.push(frame);
          else receive(frame);
          start = index + 1;
        }
        pending = bytes.slice(start);
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
  const remote = new RemoteHost(transport, {
    ...(limits.maxPending === undefined ? {} : { maxPending: limits.maxPending }),
    ...(limits.maxFrame === undefined ? {} : { maxFrame: limits.maxFrame }),
  });
  return {
    remote,
    child,
    sent,
    received,
    output,
    stderr,
    /** Reject one real write only after its bytes were delivered and cancellation can race it. */
    failSend(predicate: (frame: Frame) => boolean) {
      let reject!: (error: unknown) => void;
      let admitted!: () => void;
      const started = new Promise<void>((resolve) => {
        admitted = resolve;
      });
      const failed = new Promise<void>((_resolve, refuse) => {
        reject = refuse;
      });
      sending = (frame) => {
        if (!predicate(frame)) return;
        sending = undefined;
        admitted();
        return failed;
      };
      return { started, reject };
    },
    /** Delay selected replies without changing bytes or faking provider execution. */
    hold(predicate: (frame: Frame) => boolean) {
      filter = predicate;
    },
    /** Deliver replies after cancellation to exercise normal late-result disposal. */
    flush() {
      filter = undefined;
      for (const frame of held.splice(0)) receive(frame);
    },
    /** Close and require the real host to account for all its kernel claims and calls. */
    async close() {
      remote.close();
      const status = await Promise.race([child.exited, Bun.sleep(5000).then(() => undefined)]);
      if (status === undefined) {
        child.kill();
        await child.exited;
        throw new Error("RPC host did not settle on disconnect");
      }
      await output;
      if (status !== 0) throw new Error(await stderr);
      expect(JSON.parse(await stderr)).toEqual({ references: 0, calls: 0, roots: 0 });
    },
  };
}
