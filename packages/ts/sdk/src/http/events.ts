import { Utf8Decoder } from "./bytes.ts";

export interface ServerSentEvent {
  readonly event: string;
  readonly data: string;
  readonly id: string;
}

/** Decode SSE across UTF-8, CRLF and transport boundaries; reconnection belongs to the caller. */
export async function* events(body: AsyncIterable<Uint8Array>): AsyncIterable<ServerSentEvent> {
  const decoder = new Utf8Decoder();
  let first = true;
  let skipLf = false;
  let line = "";
  let data: string[] = [];
  let size = 0;
  let event = "";
  let id = "";
  const complete = (): ServerSentEvent | undefined => {
    if (!line) {
      const message = data.length
        ? { event: event || "message", data: data.join("\n"), id }
        : undefined;
      data = [];
      size = 0;
      event = "";
      return message;
    }
    const colon = line.indexOf(":");
    const field = colon < 0 ? line : line.slice(0, colon);
    let value = colon < 0 ? "" : line.slice(colon + 1);
    if (value.startsWith(" ")) value = value.slice(1);
    if (field === "data") {
      size += value.length + 1;
      if (size > 1 << 20) throw new Error("SSE event exceeds the limit");
      data.push(value);
    } else if (field === "event") event = value;
    else if (field === "id" && !value.includes("\0")) id = value;
    return undefined;
  };
  for await (const bytes of body) {
    for (const character of decoder.decode(bytes)) {
      if (first) {
        first = false;
        if (character === "\ufeff") continue;
      }
      if (skipLf) {
        skipLf = false;
        if (character === "\n") continue;
      }
      if (character === "\r" || character === "\n") {
        const message = complete();
        line = "";
        skipLf = character === "\r";
        if (message) yield message;
      } else {
        line += character;
        if (line.length > 65536) throw new Error("SSE line exceeds the limit");
      }
    }
  }
  // SSE dispatches on a blank line; an unfinished final event is intentionally discarded.
}
