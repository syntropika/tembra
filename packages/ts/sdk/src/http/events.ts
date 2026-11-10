import { createParser } from "eventsource-parser";
import { Utf8Decoder } from "./bytes.ts";

export interface ServerSentEvent {
  readonly event: string;
  readonly data: string;
  readonly id: string;
}

/** Adapt the maintained SSE parser to a bounded byte stream; reconnection belongs to the caller. */
export async function* events(body: AsyncIterable<Uint8Array>): AsyncIterable<ServerSentEvent> {
  const decoder = new Utf8Decoder();
  const pending: ServerSentEvent[] = [];
  let id = "";
  const parser = createParser({
    maxBufferSize: 1 << 20,
    onId(value) {
      id = value;
    },
    onEvent(event) {
      pending.push({ event: event.event ?? "message", data: event.data, id: event.id ?? id });
    },
    onError(error) {
      if (error.type === "max-buffer-size-exceeded") throw error;
    },
  });
  for await (const bytes of body) {
    parser.feed(decoder.decode(bytes));
    yield* pending;
    pending.length = 0;
  }
  parser.feed(decoder.decode(undefined, true));
  yield* pending;
}
