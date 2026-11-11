import { Buffer } from "node:buffer";
import { reply } from "./codec.ts";
import { failure as fault } from "../error.ts";
import type {
  NodeField,
  NodeIngressEvent,
  NodeInterpreterPort,
} from "@syntropika/tembra-engine-node";
import { decodeAsyncHostRequest, decodeHostRequest } from "./host-protocol.ts";
import type { IngressExchange } from "./http-ingress.ts";
import type { HttpField } from "./http-provider.ts";
import { ingressPacket, NativeIngress, type CarrierEvent } from "./native-ingress.ts";
import {
  answer,
  type ExecutionScope,
  type IngressDispatch,
  type InterpreterPort,
} from "./types.ts";

/** Native worker events cross a thread; browser execution uses the same scope directly. */
export class NodePort implements InterpreterPort {
  /** Retain one native transport until the revision physically closes. */
  constructor(readonly transport: NodeInterpreterPort) {}

  /** Read the validated source metadata once. */
  description(): string {
    return this.transport.description();
  }

  /** Release only guest-owned bookkeeping after its host environment ends. */
  release(environment: number): void {
    this.transport.release(environment);
  }

  /** Keep factory calls synchronous while decoding each host request once. */
  declare(packet: string, scope: ExecutionScope): unknown {
    return answer(
      this.transport.declare(packet, (request) =>
        respond(() => scope.sync(decodeHostRequest(request))),
      ),
    );
  }

  /** Finish the native ticket only after admitted host operations physically settle. */
  execute(packet: string, scope: ExecutionScope): Promise<string> {
    return new Promise((resolve, reject) => {
      let failure: unknown;
      const pending = new Set<Promise<unknown>>();
      // A cancelled caller ends the native callback scope, which also revokes pending standard
      // WASI calls that never pass through `scope.async`. Retained descriptors are unaffected.
      const signal = scope.signal;
      const cancel = () => this.transport.cancel();
      this.transport.dispatch(packet, (raw) => {
        try {
          const event = JSON.parse(raw);
          if (event.event === "complete") {
            signal?.removeEventListener("abort", cancel);
            scope.cancel();
            Promise.allSettled([...pending])
              .then(() => {
                try {
                  this.transport.finish();
                  if (failure !== undefined) throw failure;
                  if (!Object.hasOwn(event.result, "ok")) throw fault(event.result.error);
                  if (typeof event.result.ok !== "string")
                    throw new Error("Invalid guest result packet");
                  resolve(event.result.ok);
                } catch (error) {
                  reject(error);
                }
              })
              .catch(reject);
          } else if (event.event === "host") {
            if (!event.asynchronous)
              this.transport.reply(
                event.id,
                respond(() => scope.sync(decodeHostRequest(event.packet))),
              );
            else {
              let request!: ReturnType<typeof decodeAsyncHostRequest>;
              const operation = Promise.resolve()
                .then(() => {
                  request = decodeAsyncHostRequest(event.packet);
                  return scope.async(request);
                })
                .then(
                  (value) =>
                    scope.deliver(request, value, (value) =>
                      this.transport.reply(
                        event.id,
                        respond(() => value),
                      ),
                    ),
                  (error) =>
                    this.transport.reply(
                      event.id,
                      respond(() => {
                        throw error;
                      }),
                    ),
                )
                .catch((error) => {
                  failure = error;
                });
              pending.add(operation);
              operation.finally(() => pending.delete(operation));
            }
          } else throw new Error("Unknown interpreter event");
        } catch (error) {
          failure = error;
        }
      });
      if (signal?.aborted) cancel();
      else signal?.addEventListener("abort", cancel, { once: true });
    });
  }

  /**
   * Serve one incoming HTTP request through the typed NAPI carrier. Bytes cross as `Buffer`s;
   * the head is published early and this resolves only after `complete`, the drain of admitted
   * host work and `finish()`.
   */
  ingress(
    dispatch: IngressDispatch,
    exchange: IngressExchange,
    scope: ExecutionScope,
  ): Promise<void> {
    const transport = this.transport;
    const carrier = new NativeIngress(
      {
        supply: (answer) =>
          answer.kind === "data"
            ? transport.supplyRequest(answer.data, null, null)
            : answer.kind === "end"
              ? transport.supplyRequest(null, answer.trailers.map(nodeField), null)
              : transport.supplyRequest(null, null, answer.message),
        pull: () => transport.pullResponse(),
        disconnect: () => transport.disconnect(),
        cancel: () => transport.cancel(),
        reply: (id, packet) => transport.reply(id, packet),
        finish: () => transport.finish(),
      },
      exchange,
      scope,
    );
    const { method, scheme, authority, pathWithQuery, fields } = exchange.request;
    try {
      transport.ingress(
        ingressPacket(dispatch),
        {
          method,
          ...(scheme === undefined ? {} : { scheme }),
          ...(authority === undefined ? {} : { authority }),
          ...(pathWithQuery === undefined ? {} : { pathWithQuery }),
          fields: fields.map(nodeField),
        },
        (event) => {
          try {
            carrier.event(carrierEvent(event));
          } catch (error) {
            // A malformed event cannot unwind the native callback; the call fails instead.
            carrier.fail(error);
          }
        },
      );
    } catch (error) {
      // Refused before admission (an invalid head, a closed port): nothing native began.
      void carrier.abandon(error);
      return carrier.settled;
    }
    carrier.begun();
    return carrier.settled;
  }

  /** Drop an idle native Store after the common lifecycle releases its scopes. */
  close(): void {
    this.transport.close();
  }
}

/** Encode one native response without throwing through a callback frame. */
function respond(operation: () => unknown): string {
  try {
    return `{ "ok":${reply(operation())}}`;
  } catch (error) {
    return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
  }
}

/** A field as a `Buffer` view of the same bytes; NAPI copies it. */
function nodeField([name, value]: HttpField): NodeField {
  return { name, value: Buffer.from(value.buffer, value.byteOffset, value.byteLength) };
}

/** A NAPI `Buffer` as a plain `Uint8Array` view, without copying. */
function bytes(value: Uint8Array): Uint8Array {
  return new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
}

function fields(value: readonly NodeField[]): readonly HttpField[] {
  return value.map((field) => [field.name, bytes(field.value)] as const);
}

/** The NAPI event in the shared carrier form. */
function carrierEvent(event: NodeIngressEvent): CarrierEvent {
  switch (event.kind) {
    case "refused":
      return { kind: "refused", code: bytes(event.code) };
    case "data":
      return { kind: "data", data: bytes(event.data) };
    case "head":
      return { kind: "head", status: event.status, fields: fields(event.fields) };
    case "trailers":
      return { kind: "trailers", fields: fields(event.fields) };
    default:
      return event;
  }
}
