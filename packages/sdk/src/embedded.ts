import { Errors } from "./errors.ts";
import { Code, HookCapability, Lifetime, Reason, ReloadStatus, Status } from "./identifiers.ts";
import { Host } from "./host.ts";
import { jsonCodec } from "./codec.ts";
import { TembraError } from "./error.ts";
import type { HostOptions } from "./types.ts";

declare const Deno: { core: { ops: Record<string, (...args: any[]) => any> } };
const ops = Deno.core.ops;
const globals = globalThis as any;

// Embedded V8 has no browser or Node dependency. Scheduling belongs to this adapter.
globals.queueMicrotask ??= (callback: () => void) => Promise.resolve().then(callback);
globals.performance ??= { now: () => ops.op_tembra_now!() };
globals.AbortController ??= class {
  signal = {
    aborted: false,
    reason: undefined as unknown,
    listeners: new Set<any>(),
    addEventListener(_type: string, callback: any) {
      this.listeners.add(callback);
    },
    removeEventListener(_type: string, callback: any) {
      this.listeners.delete(callback);
    },
    throwIfAborted() {
      if (this.aborted) throw this.reason;
    },
  };
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
  function answer(bytes: Uint8Array): any {
    const envelope = codec.deserialize(bytes) as any;
    if (envelope.error) throw new TembraError(envelope.error);
    return envelope.ok;
  }
  return new Host({
    ...options,
    kernel: { request: (metadata) => ops.op_tembra_kernel!(metadata) },
    native: {
      hasFacet: (id) => ops.op_tembra_has_facet!(id),
      activate: async (object, facet, service) => {
        answer(
          await ops.op_tembra_native!("activate", codec.serialize({ object, facet, service })),
        );
      },
      invoke: async (invocation, arguments_) =>
        answer(
          await ops.op_tembra_native!(
            "invoke",
            codec.serialize({ invocation, arguments: arguments_ }),
          ),
        ),
      dispose: async (object) => {
        answer(await ops.op_tembra_native!("dispose", codec.serialize({ object })));
      },
      effect: async (effect) =>
        answer(await ops.op_tembra_native!("effect", codec.serialize(effect))),
      cancelEffect: async (effect) => {
        answer(await ops.op_tembra_native!("cancel-effect", codec.serialize(effect)));
      },
      value: (id) => answer(ops.op_tembra_value!(id)),
      dropValue: (id) => ops.op_tembra_drop_value!(id),
    },
  });
}

globals.Tembra = Object.freeze({
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
