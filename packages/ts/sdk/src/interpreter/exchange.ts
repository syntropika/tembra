import type { InterpreterPort } from "./types.ts";
import { answer } from "./types.ts";

/** Bind packets to one already issued host scope, including its asynchronous physical work. */
export interface Scope {
  readonly signal?: AbortSignal;
  check(): void;
  cancel(): void;
  sync(packet: string): unknown;
  async(packet: string): Promise<unknown>;
}
/** Never throw through a native callback frame; errors belong to the original request. */
export function respond(operation: () => unknown): string {
  try {
    return JSON.stringify({ ok: operation() ?? null });
  } catch (error) {
    return JSON.stringify({ error: error instanceof Error ? error.message : String(error) });
  }
}
/** Execute a synchronous factory or validator without exposing an event-loop capability. */
export async function declare(
  port: InterpreterPort,
  packet: unknown,
  scope: Scope,
): Promise<unknown> {
  return answer(
    await port.declare(JSON.stringify(packet), (request) => respond(() => scope.sync(request))),
  );
}
/** Retain host operations until they settle; only the worker's completion reopens its admission. */
export function dispatch(port: InterpreterPort, packet: unknown, scope: Scope): Promise<unknown> {
  return new Promise((resolve, reject) => {
    let failure: unknown;
    port.dispatch(JSON.stringify(packet), (raw) => {
      try {
        const event = JSON.parse(raw);
        if (event.event === "complete") {
          try {
            scope.cancel();
            port.finish();
            if (failure !== undefined) throw failure;
            resolve(answer(JSON.stringify(event.result)));
          } catch (error) {
            reject(error);
          }
        } else if (event.event === "host") {
          if (!event.asynchronous)
            port.reply(
              event.id,
              respond(() => scope.sync(event.packet)),
            );
          else
            scope
              .async(event.packet)
              .then(
                (value) =>
                  port.reply(
                    event.id,
                    respond(() => value),
                  ),
                (error) =>
                  port.reply(
                    event.id,
                    respond(() => {
                      throw error;
                    }),
                  ),
              )
              .catch((error) => {
                failure = error;
              });
        } else throw new Error("Unknown interpreter event");
      } catch (error) {
        failure = error;
      }
    });
  });
}
