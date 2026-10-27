import { Code } from "../../bridge/identifiers.ts";
import { RpcEvent, RpcOp } from "../../bridge/protocol.ts";
import type { Codec } from "../../codecs/json.ts";
import { jsonCodec } from "../../codecs/json.ts";
import { failure, TembraError } from "../../error.ts";
import { Errors } from "../../errors.ts";

/** Adapters for WebSocket, MessagePort, or an application channel implement this byte boundary. */
export interface Transport {
  /** Send one encoded frame through the application transport. */
  send(frame: Uint8Array): void | Promise<void>;
  /** Register frame and connection callbacks until detached. */
  listen(receive: (frame: Uint8Array) => void, closed: (error?: unknown) => void): () => void;
  /**
   * Close after channel listeners detach, handling or ignoring the supplied terminal cause.
   * Wrap platform methods; do not forward this cause to WebSocket.close(code), destroy(error), or end.
   */
  close(cause?: unknown): void;
}
export interface Limits {
  codec?: Codec;
  maxFrame?: number;
  maxPending?: number;
  onError?: (error: unknown) => void;
}
export interface Wire {
  id?: number;
  op?: RpcOp;
  payload?: any;
  ok?: any;
  error?: { code: Code; reason: string; message: string };
  event?: RpcEvent;
}
/** Produce the portable fields of a structured error. */
export function errorValue(error: unknown) {
  const value = failure(error);
  return { code: value.code, reason: value.reason, message: value.message };
}

export class Channel {
  readonly codec: Codec;
  readonly maxFrame: number;
  readonly maxPending: number;
  closed = false;
  #detach: () => void;

  /** Attach the transport with frame limits and the chosen codec. */
  constructor(
    readonly transport: Transport,
    readonly options: Limits,
    receive: (message: Wire) => void,
    readonly ended: (error: unknown) => void,
  ) {
    this.codec = options.codec ?? jsonCodec;
    this.maxFrame = options.maxFrame ?? 1024 * 1024;
    this.maxPending = options.maxPending ?? 128;
    this.#detach = transport.listen(
      (frame) => {
        if (this.closed) return;
        try {
          if (frame.length > this.maxFrame) throw new TembraError(Errors.Exhausted.FrameSize);
          const message = this.codec.deserialize(frame);
          if (!message || typeof message !== "object" || Array.isArray(message))
            throw new TembraError(Errors.Invalid.RpcFrame);
          receive(message);
        } catch (error) {
          this.#report(error);
          this.close(error);
        }
      },
      (error) => this.close(error),
    );
  }

  /** Report adapter failures without letting a reporter prevent connection cleanup. */
  #report(error: unknown): void {
    try {
      this.options.onError?.(error);
    } catch {}
  }

  /** Encode and size-check a message before transport delivery. */
  async send(message: Wire): Promise<void> {
    if (this.closed) throw new TembraError(Errors.Gone.Connection);
    const frame = this.codec.serialize(message);
    if (frame.length > this.maxFrame) throw new TembraError(Errors.Exhausted.FrameSize);
    await this.transport.send(frame);
  }

  /** Detach the transport and reject connection work once. */
  close(error: unknown = new TembraError(Errors.Gone.Connection)): void {
    if (this.closed) return;
    this.closed = true;
    try {
      this.#detach?.();
    } catch (detachError) {
      this.#report(detachError);
    }
    try {
      this.transport.close(error);
    } catch (closeError) {
      this.#report(closeError);
    } finally {
      this.ended(error);
    }
  }
}
