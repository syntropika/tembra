import type { TransportStream } from "../http.ts";

/** Retain one transport chunk while separating framing from body bytes. */
export class Reader {
  #buffer: Uint8Array = new Uint8Array();
  #offset = 0;
  constructor(readonly stream: TransportStream) {}

  private async available(): Promise<boolean> {
    while (this.#offset === this.#buffer.length) {
      const bytes = await this.stream.read();
      if (bytes === null) return false;
      if (!(bytes instanceof Uint8Array) || bytes.length > 16384 || !bytes.length)
        throw new Error("Invalid transport byte chunk");
      this.#buffer = bytes;
      this.#offset = 0;
    }
    return true;
  }

  async read(max = 16384): Promise<Uint8Array | null> {
    if (!(await this.available())) return null;
    const end = Math.min(this.#buffer.length, this.#offset + max);
    const bytes = this.#buffer.subarray(this.#offset, end);
    this.#offset = end;
    return bytes;
  }

  async line(limit = 8192): Promise<string> {
    let line = "";
    for (;;) {
      if (!(await this.available())) throw new Error("Truncated HTTP framing");
      const lf = this.#buffer.indexOf(10, this.#offset);
      const end = lf < 0 ? this.#buffer.length : lf;
      if (line.length + end - this.#offset > limit + 1)
        throw new Error("HTTP framing line exceeds the limit");
      line += String.fromCharCode(...this.#buffer.subarray(this.#offset, end));
      this.#offset = lf < 0 ? end : end + 1;
      if (lf >= 0) {
        if (!line.endsWith("\r")) throw new Error("HTTP framing requires CRLF");
        return line.slice(0, -1);
      }
    }
  }
}

/** UTF-8 codecs use ECMAScript primitives available in every supported guest engine. */
export function encodeText(text: string): Uint8Array {
  const bytes: number[] = [];
  for (const character of text) {
    let point = character.codePointAt(0)!;
    if (point >= 0xd800 && point <= 0xdfff) point = 0xfffd;
    if (point < 0x80) bytes.push(point);
    else if (point < 0x800) bytes.push(0xc0 | (point >> 6), 0x80 | (point & 63));
    else if (point < 0x10000)
      bytes.push(0xe0 | (point >> 12), 0x80 | ((point >> 6) & 63), 0x80 | (point & 63));
    else
      bytes.push(
        0xf0 | (point >> 18),
        0x80 | ((point >> 12) & 63),
        0x80 | ((point >> 6) & 63),
        0x80 | (point & 63),
      );
  }
  return Uint8Array.from(bytes);
}

/** Replace invalid UTF-8 sequences and retain at most one partial scalar between chunks. */
export class Utf8Decoder {
  #pending: number[] = [];

  decode(bytes: Uint8Array = new Uint8Array(), final = false): string {
    const input = this.#pending.length ? Uint8Array.from([...this.#pending, ...bytes]) : bytes;
    this.#pending = [];
    const output: string[] = [];
    for (let index = 0; index < input.length;) {
      const first = input[index]!;
      const width =
        first < 0x80
          ? 1
          : first >= 0xc2 && first <= 0xdf
            ? 2
            : first >= 0xe0 && first <= 0xef
              ? 3
              : first >= 0xf0 && first <= 0xf4
                ? 4
                : 0;
      if (!width) {
        output.push("\ufffd");
        index++;
        continue;
      }
      if (index + width > input.length && !final) {
        this.#pending = [...input.subarray(index)];
        break;
      }
      let point = first & (width === 1 ? 127 : (1 << (7 - width)) - 1);
      let count = 1;
      while (count < width) {
        const next = input[index + count];
        if (
          next === undefined ||
          (next & 0xc0) !== 0x80 ||
          (count === 1 &&
            ((first === 0xe0 && next < 0xa0) ||
              (first === 0xed && next >= 0xa0) ||
              (first === 0xf0 && next < 0x90) ||
              (first === 0xf4 && next >= 0x90)))
        )
          break;
        point = (point << 6) | (next & 63);
        count++;
      }
      output.push(count === width ? String.fromCodePoint(point) : "\ufffd");
      index += count;
    }
    return output.join("");
  }
}

export async function decodeText(body: AsyncIterable<Uint8Array>, limit: number): Promise<string> {
  const decoder = new Utf8Decoder();
  const output: string[] = [];
  let bytes = 0;
  for await (const chunk of body) {
    bytes += chunk.length;
    if (bytes > limit)
      throw new Error("HTTP text body exceeds the limit; consume the byte stream instead");
    output.push(decoder.decode(chunk));
  }
  output.push(decoder.decode(undefined, true));
  return output.join("");
}
