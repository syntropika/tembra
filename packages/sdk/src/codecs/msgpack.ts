import { decode, encode, ExtensionCodec } from "@msgpack/msgpack";
import { CodecId } from "../bridge/protocol.ts";
import type { Codec } from "./json.ts";
import { text, utf8 } from "./json.ts";

const extensions = new ExtensionCodec();
extensions.register({
  type: 1,
  /** Encode arbitrary-size integers as decimal UTF-8 text. */
  encode(value) {
    return typeof value === "bigint" ? utf8(value.toString()) : null;
  },
  /** Restore a BigInt without passing through a floating-point number. */
  decode(bytes) {
    return BigInt(text(bytes));
  },
});

/** Numbers remain Numbers; extension 1 preserves arbitrary BigInts independently of integer size. */
export const messagePackCodec: Codec = Object.freeze({
  id: CodecId.MessagePack,
  /** Encode portable data with the shared BigInt extension. */
  serialize: (value: unknown) => encode(value, { extensionCodec: extensions }),
  /** Restore portable data with the matching extension codec. */
  deserialize: (bytes: Uint8Array) => decode(bytes, { extensionCodec: extensions }),
});
