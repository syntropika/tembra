import type { DynamicReference } from "../../authoring/types.ts";
import { RpcEvent, RpcOp } from "../../bridge/protocol.ts";
import { TembraError } from "../../error.ts";
import { Errors } from "../../errors.ts";
import type { Host } from "../../host.ts";
import type { Context } from "../../runtime/context.ts";

import { Channel, errorValue, type Limits, type Transport } from "./channel.ts";

/** Trusted infrastructure supplies the principal and authorization policy; peers cannot choose it. */
export function serve(
  host: Host,
  transport: Transport,
  options: Limits & {
    principal: string;
    allow?: (service: string, key: readonly string[]) => boolean;
  },
): {
  /** Stop the connection and release its remote claims. */
  close(): void;
} {
  type Claim = {
    reference: DynamicReference;
    methods: ReadonlySet<string>;
    detach?: () => void;
    pending?: unknown;
    sending: boolean;
    closed: boolean;
    active: boolean;
  };
  const references = new Map<number, Claim>();
  const calls = new Map<number, Context>();
  const requests = new Map<number, { cancelled: boolean }>();
  let next = 0;
  let pending = 0;
  const pump = (reference: number, record: Claim) => {
    if (record.sending || !record.active || record.closed) return;
    record.sending = true;
    void (async () => {
      while (Object.hasOwn(record, "pending") && !record.closed) {
        const value = record.pending;
        delete record.pending;
        await channel.send({ event: RpcEvent.State, payload: { reference, value } });
      }
    })()
      .catch((error) => channel.close(error))
      .finally(() => {
        record.sending = false;
      });
  };
  const channel = new Channel(
    transport,
    options,
    (message) => {
      if (message.op === RpcOp.Cancel) {
        const request = requests.get(message.payload?.id);
        if (request) request.cancelled = true;
        calls.get(message.payload?.id)?.cancel();
        return;
      }
      const id = message.id;
      if (!Number.isSafeInteger(id) || !id || id <= 0 || !message.op) {
        channel.close(new TembraError(Errors.Invalid.RpcRequest));
        return;
      }
      if (pending >= channel.maxPending || requests.has(id)) {
        void channel
          .send({ id, error: errorValue(new TembraError(Errors.Exhausted.PendingRequests)) })
          .catch(() => channel.close());
        return;
      }
      pending++;
      const request = { cancelled: false };
      requests.set(id, request);
      void (async () => {
        const payload = message.payload;
        switch (message.op) {
          case RpcOp.Describe:
            return host.installedOrbs().map(({ describe, ...info }) => info);
          case RpcOp.Get: {
            if (
              typeof payload?.service !== "string" ||
              !Array.isArray(payload.key) ||
              !payload.key.every((key: unknown) => typeof key === "string")
            )
              throw new TembraError(Errors.Invalid.Address);
            if (options.allow && !options.allow(payload.service, payload.key))
              throw new TembraError(Errors.Forbidden.Service);
            if (references.size >= channel.maxPending)
              throw new TembraError(Errors.Exhausted.RemoteReferences);
            const reference = await host.get(payload.service, payload.key, {
              principal: options.principal,
            });
            if (channel.closed || request.cancelled) {
              reference.release();
              throw new TembraError(Errors.Gone.Connection);
            }
            const referenceId = ++next;
            const methods = new Set(host.session.handles.reference(reference).methods);
            const record: Claim = {
              reference,
              methods,
              sending: false,
              closed: false,
              active: false,
            };
            references.set(referenceId, record);
            let snapshot: { state?: unknown } = {};
            try {
              snapshot = { state: reference.state.value };
              let initial = true;
              record.detach = reference.state.subscribe((value) => {
                if (initial) {
                  initial = false;
                  return;
                }
                record.pending = value;
                pump(referenceId, record);
              });
            } catch {
              /* State is optional; method-only services remain portable. */
            }
            return { reference: referenceId, methods: [...methods], ...snapshot };
          }
          case RpcOp.Release: {
            const record = references.get(payload?.reference);
            if (record) {
              record.closed = true;
              record.detach?.();
              record.reference.release();
              references.delete(payload.reference);
            }
            return null;
          }
          case RpcOp.Call: {
            const record = references.get(payload?.reference);
            if (
              !record ||
              typeof payload.method !== "string" ||
              !record.methods.has(payload.method) ||
              !Array.isArray(payload.arguments) ||
              typeof record.reference[payload.method] !== "function"
            )
              throw new TembraError(Errors.Forbidden.Method);
            if (request.cancelled) throw new TembraError(Errors.Cancelled.Request);
            return host.root(
              async (ctx) => {
                calls.set(id, ctx);
                try {
                  return (
                    (await record.reference[payload.method](...payload.arguments, ctx)) ?? null
                  );
                } finally {
                  calls.delete(id);
                }
              },
              { principal: options.principal },
            );
          }
          default:
            throw new TembraError(Errors.Invalid.RpcOperation);
        }
      })()
        .then(
          async (ok) => {
            await channel.send({ id, ok });
            if (message.op === RpcOp.Get) {
              const record = references.get(ok.reference);
              if (record) {
                record.active = true;
                pump(ok.reference, record);
              }
            }
          },
          (error) => channel.send({ id, error: errorValue(error) }),
        )
        .catch((error) => channel.close(error))
        .finally(() => {
          pending--;
          requests.delete(id);
        });
    },
    () => {
      for (const ctx of calls.values()) {
        try {
          ctx.cancel();
        } catch {}
      }
      for (const record of references.values()) {
        record.closed = true;
        record.detach?.();
        record.reference.release();
      }
      references.clear();
    },
  );
  return {
    /** Stop the transport and release its remote claims. */
    close: () => channel.close(),
  };
}
