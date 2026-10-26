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
import { TembraError } from "./error.ts";
import { ContextKey, ContextKind, opaqueFrom } from "./context-key.ts";
import { Errors } from "./errors.ts";
import { Host } from "./host.ts";

declare const Deno: { core: { ops: Record<string, (...args: any[]) => any> } };
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
  return new Host({
    ...options,
    kernel: {
      /** Exchange kernel metadata without encoding JS payload values. */
      request: (metadata) => ops.op_tembra_kernel!(metadata),
    },
    native: {
      /** Check whether this realm must delegate the facet to Rust. */
      hasFacet: (id) => ops.op_tembra_has_facet!(id),
      /** Declare native scopes before the shared kernel activates them. */
      activate: async (object, facet, service) => {
        answer(
          await ops.op_tembra_native!("activate", codec.serialize({ object, facet, service })),
        );
      },
      /** Encode only the call arguments that cross into Rust. */
      invoke: async (invocation, arguments_) =>
        answer(
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
