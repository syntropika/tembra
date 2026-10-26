import type { HostOptions } from "./authoring/types.ts";
import {
  Code,
  HookCapability,
  Lifetime,
  Reason,
  ReloadStatus,
  Status,
} from "./bridge/identifiers.ts";
import { jsonCodec } from "./codecs/json.ts";
import { failure, TembraError } from "./error.ts";
import { ContextKey, ContextKind, opaqueFrom } from "./context-key.ts";
import { Errors } from "./errors.ts";
import { Host } from "./host.ts";
import { Context } from "./runtime/context.ts";
import { KernelOp } from "./bridge/protocol.ts";
import type { Invocation } from "./bridge/kernel.ts";

declare const Deno: {
  core: {
    ops: Record<string, (...args: any[]) => any>;
    unrefOpPromise(promise: Promise<unknown>): void;
  };
};
const ops = Deno.core.ops;
const globals = globalThis as any;

// Embedded V8 has no browser or Node dependency. Scheduling belongs to this adapter.
globals.queueMicrotask ??= (callback: () => void) => Promise.resolve().then(callback);
globals.performance ??= {
  /** Use the Rust runtime's monotonic clock. */
  now: () => ops.op_tembra_now!(),
};
globals.AbortController ??= class {
  signal = {
    aborted: false,
    reason: undefined as unknown,
    listeners: new Set<any>(),
    /** Retain a cancellation listener until it is removed or fired. */
    addEventListener(_type: string, callback: any) {
      this.listeners.add(callback);
    },
    /** Release a cancellation listener without invoking it. */
    removeEventListener(_type: string, callback: any) {
      this.listeners.delete(callback);
    },
    /** Reject work after cancellation has been signalled. */
    throwIfAborted() {
      if (this.aborted) throw this.reason;
    },
  };

  /** Signal cancellation once without waiting for physical work. */
  abort(reason: unknown = new Error("Aborted")) {
    if (this.signal.aborted) return;
    this.signal.aborted = true;
    this.signal.reason = reason;
    for (const callback of [...this.signal.listeners]) callback();
    this.signal.listeners.clear();
  }
};
let nextTimer = 0;
const timers = new Set<number>();
globals.setTimeout ??= (callback: (...args: unknown[]) => void, delay = 0, ...args: unknown[]) => {
  const id = ++nextTimer;
  timers.add(id);
  void ops.op_tembra_sleep!(id, Math.max(0, Math.min(0x7fff_ffff, delay))).then(() => {
    if (timers.delete(id)) callback(...args);
  });
  return id;
};
globals.clearTimeout ??= (id: number) => {
  if (timers.delete(id)) ops.op_tembra_clear_timer!(id);
};

/** The same JS adapter used by Node and browsers, backed by injected Rust operations. */
function createHost(options: Omit<HostOptions, "kernel" | "native"> = {}): Host {
  const codec = options.serialization ?? jsonCodec;
  if (codec.id !== ops.op_tembra_codec!())
    throw new TembraError(Errors.Conflict.Codec, "The Rust and JS host codecs must agree");
  /** Decode native callback results with the agreed host codec. */
  function answer(bytes: Uint8Array): any {
    const envelope = codec.deserialize(bytes) as any;
    if (envelope.error) throw new TembraError(envelope.error);
    return envelope.ok;
  }
  const cooperative = ops.op_tembra_driver_enabled?.() ?? false;
  const pending = new Map<
    number,
    { resolve(value: unknown): void; reject(error: unknown): void }
  >();
  globals.__tembraDeliver = (batch: [number, Uint8Array | string, boolean][]) => {
    for (const [id, payload, success] of batch) {
      const waiter = pending.get(id);
      if (!waiter) throw new Error("unknown native completion");
      pending.delete(id);
      try {
        if (!success) throw new Error(payload as string);
        waiter.resolve(answer(payload as Uint8Array));
      } catch (error) {
        waiter.reject(error);
      }
    }
  };
  function submit(operation: string, payload: Uint8Array): Promise<any> {
    const id = ops.op_tembra_submit!(operation, payload);
    return new Promise((resolve, reject) => pending.set(id, { resolve, reject }));
  }
  const host = new Host({
    ...options,
    clockOrigin: 0,
    kernel: {
      /** Exchange kernel metadata without encoding JS payload values. */
      request: (metadata) => ops.op_tembra_kernel!(metadata),
    },
    native: {
      ...(cooperative
        ? {
            admit: (reference: number, method: string, ctx: Context) => {
              const result = JSON.parse(
                ops.op_tembra_admit!(
                  JSON.stringify({
                    reference,
                    method,
                    root: ctx.root,
                    context: ctx.frame,
                    parent: ctx.invocation?.id,
                  }),
                ),
              ) as any;
              if (result.error) throw new TembraError(result.error);
              return result.ok;
            },
            finish: (call: number, now: number) => {
              const result = JSON.parse(ops.op_tembra_finish!(call, String(now))) as any;
              if (result.error) throw new TembraError(result.error);
              return result.ok;
            },
          }
        : {}),
      rootEnded: (root) => ops.op_tembra_end_context!(root),
      access: (orb, service, principal) =>
        JSON.parse(ops.op_tembra_access!(orb, service, principal)) ?? undefined,
      settled: () => ops.op_tembra_settled!(),
      notify: (action) => ops.op_tembra_notify!(JSON.stringify(action)),
      hasContribution: (operation) => ops.op_tembra_has_contribution?.(operation) ?? false,
      contribute: async (operation, ctx, input) => {
        const result = answer(
          await ops.op_tembra_native!(
            "contribute",
            codec.serialize({
              operation,
              root: ctx.root,
              frame: ctx.frame,
              ...(input === undefined ? {} : { input }),
            }),
          ),
        );
        return result.value;
      },
      /** Check whether this realm must delegate the orb to Rust. */
      hasOrb: (id) => ops.op_tembra_has_orb!(id),
      /** Declare native scopes before the shared kernel activates them. */
      activate: async (object, orb, service) => {
        answer(await ops.op_tembra_native!("activate", codec.serialize({ object, orb, service })));
      },
      /** Encode only the call arguments that cross into Rust. */
      invoke: async (invocation, arguments_) =>
        cooperative
          ? submit("invoke", codec.serialize({ invocation, arguments: arguments_ }))
          : answer(
              await ops.op_tembra_native!(
                "invoke",
                codec.serialize({ invocation, arguments: arguments_ }),
              ),
            ),
      /** Release native object storage after shared scope cleanup. */
      dispose: async (object) => {
        answer(await ops.op_tembra_native!("dispose", codec.serialize({ object })));
      },
      /** Delegate callback execution while Rust retains lifecycle ordering. */
      effect: async (effect) =>
        answer(await ops.op_tembra_native!("effect", codec.serialize(effect))),
      /** Signal native cancellation without acknowledging physical settlement. */
      cancelEffect: async (effect) => {
        answer(await ops.op_tembra_native!("cancel-effect", codec.serialize(effect)));
      },
      /** Decode one native bridge value with the agreed codec. */
      value: (id) => answer(ops.op_tembra_value!(id)),
      /** Release the native value selected by a kernel action. */
      dropValue: (id) => ops.op_tembra_drop_value!(id),
    },
  });
  if (ops.op_tembra_callback_next) {
    // Receiving is idle infrastructure; each callback is separately tracked physical work.
    void (async () => {
      for (;;) {
        const receiving = ops.op_tembra_callback_next!();
        Deno.core.unrefOpPromise(receiving);
        const packet = (await receiving) as Uint8Array;
        if (!packet.length) return;
        const header = new DataView(packet.buffer, packet.byteOffset, 9);
        const kind = header.getUint8(0);
        const id = header.getUint32(1, true) + header.getUint32(5, true) * 2 ** 32;
        if (kind !== 0) {
          if (kind === 2) host.session.calls.releaseRoot(id);
          if (kind === 3) host.session.handles.revoke(id);
          host.session.lifecycle.flush();
          continue;
        }
        let request: any;
        try {
          request = codec.deserialize(packet.subarray(9));
        } catch (error) {
          const { code, reason, message } = failure(error);
          ops.op_tembra_callback_error!(id, JSON.stringify({ code, reason, message }), true);
          continue;
        }
        host.session.track(
          (async () => {
            let envelope: unknown;
            try {
              let result: unknown;
              if (request.kind === "activate") {
                await host.session.handles.activateAcquired(
                  request.acquired,
                  request.address,
                  request.methods,
                );
                result = null;
              } else {
                const call = request.call;
                const invocation = host.session.kernel.command<Invocation>(KernelOp.Invocation, {
                  call,
                });
                const ctx = new Context(host.session, invocation.root, invocation, request.frame);
                if (request.kind === "invoke") {
                  const object = host.session.objects.get(invocation.object);
                  const environment = object?.versions.get(invocation.orb);
                  const handle =
                    environment?.orb.definition.services?.[object!.address.service]?.methods[
                      invocation.method
                    ];
                  if (!handle || !environment) throw new TembraError(Errors.Gone.Implementation);
                  result = await handle(environment.value, ctx, ...request.input);
                } else {
                  result = await host.session.orbs.runContribution(
                    request.operation,
                    ctx,
                    request.input,
                  );
                }
              }
              envelope = result === undefined ? {} : { value: result };
            } catch (error) {
              const normalized = failure(error);
              envelope = {
                error: {
                  code: normalized.code,
                  reason: normalized.reason,
                  message: normalized.message,
                },
              };
            }
            try {
              ops.op_tembra_callback_complete!(id, codec.serialize(envelope));
            } catch (error) {
              const { code, reason, message } = failure(error);
              ops.op_tembra_callback_error!(id, JSON.stringify({ code, reason, message }), false);
            }
          })(),
          true,
        );
      }
    })();
  }
  return host;
}

globals.Tembra = Object.freeze({
  ContextKey,
  ContextKind,
  opaqueFrom,
  createHost,
  jsonCodec,
  Code,
  Reason,
  Lifetime,
  Status,
  ReloadStatus,
  HookCapability,
  Errors,
});
