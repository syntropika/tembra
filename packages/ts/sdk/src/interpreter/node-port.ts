import { reply } from "./codec.ts";
import { failure as fault } from "../error.ts";
import type { NodeInterpreterPort } from "@syntropika/tembra-engine-node";
import { decodeAsyncHostRequest, decodeHostRequest, type IORequest } from "./host-protocol.ts";
import { answer, withIoCancellation, type ExecutionScope, type InterpreterPort } from "./types.ts";

/** Native worker events cross a thread; browser execution uses the same scope directly. */
export class NodePort implements InterpreterPort {
  /** Retain one native transport until the revision physically closes. */
  constructor(readonly transport: NodeInterpreterPort) {}

  /** Native directory, socket and HTTP capabilities execute on the host's I/O reactor. */
  async io(request: IORequest, signal?: AbortSignal): Promise<unknown> {
    return JSON.parse(
      await withIoCancellation(this.transport.io(JSON.stringify(request)), request, signal, () =>
        this.transport.cancelIo(),
      ),
    );
  }

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
      const cancel = () => this.transport.cancelIo();
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
              const operation = Promise.resolve()
                .then(() => scope.async(decodeAsyncHostRequest(event.packet)))
                .then(
                  (value) =>
                    this.transport.reply(
                      event.id,
                      respond(() => value),
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
