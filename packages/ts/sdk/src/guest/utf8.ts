// Strict UTF-8 for guest author helpers. The embedded QuickJS runtime has no TextEncoder or
// TextDecoder, so this module never assumes them.

/** Code units converted per `String.fromCharCode` call, below every engine's argument limit. */
const BATCH = 8192;

/** Encode `text` as UTF-8 into one exact Uint8Array. */
export function utf8Encode(text: string): Uint8Array {
  const out = new Uint8Array(text.length * 3);
  let length = 0;
  for (let index = 0; index < text.length; index++) {
    let code = text.charCodeAt(index);
    if (code >= 0xd800 && code <= 0xdfff) {
      const next = text.charCodeAt(index + 1);
      if (code <= 0xdbff && next >= 0xdc00 && next <= 0xdfff) {
        code = 0x10000 + ((code - 0xd800) << 10) + (next - 0xdc00);
        index++;
      } else code = 0xfffd;
    }
    if (code < 0x80) out[length++] = code;
    else if (code < 0x800) {
      out[length++] = 0xc0 | (code >> 6);
      out[length++] = 0x80 | (code & 63);
    } else if (code < 0x10000) {
      out[length++] = 0xe0 | (code >> 12);
      out[length++] = 0x80 | ((code >> 6) & 63);
      out[length++] = 0x80 | (code & 63);
    } else {
      out[length++] = 0xf0 | (code >> 18);
      out[length++] = 0x80 | ((code >> 12) & 63);
      out[length++] = 0x80 | ((code >> 6) & 63);
      out[length++] = 0x80 | (code & 63);
    }
  }
  return out.slice(0, length);
}

/** Decode strict UTF-8; any invalid sequence throws `RangeError`. */
export function utf8Decode(bytes: Uint8Array): string {
  let text = "";
  const units: number[] = [];
  for (let index = 0; index < bytes.length;) {
    const first = bytes[index++]!;
    let code: number;
    if (first < 0x80) code = first;
    else {
      let extra: number;
      let minimum: number;
      if (first >= 0xc2 && first <= 0xdf) [code, extra, minimum] = [first & 31, 1, 0x80];
      else if (first >= 0xe0 && first <= 0xef) [code, extra, minimum] = [first & 15, 2, 0x800];
      else if (first >= 0xf0 && first <= 0xf4) [code, extra, minimum] = [first & 7, 3, 0x10000];
      else throw new RangeError("invalid UTF-8");
      for (; extra > 0; extra--) {
        const next = bytes[index++];
        if (next === undefined || (next & 0xc0) !== 0x80) throw new RangeError("invalid UTF-8");
        code = (code << 6) | (next & 63);
      }
      if (code < minimum || code > 0x10ffff || (code >= 0xd800 && code <= 0xdfff))
        throw new RangeError("invalid UTF-8");
    }
    if (code < 0x10000) units.push(code);
    else units.push(0xd800 + ((code - 0x10000) >> 10), 0xdc00 + ((code - 0x10000) & 0x3ff));
    if (units.length >= BATCH) {
      text += String.fromCharCode.apply(null, units);
      units.length = 0;
    }
  }
  return text + String.fromCharCode.apply(null, units);
}
