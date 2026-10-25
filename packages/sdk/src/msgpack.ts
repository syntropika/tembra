import { CodecId } from "./protocol.ts";
import { decode, encode, ExtensionCodec } from "@msgpack/msgpack";
import type { Codec } from "./codec.ts";
import { text, utf8 } from "./codec.ts";

const extensions = new ExtensionCodec();
extensions.register({
  type: 1,
  encode(value) {
    return typeof value === "bigint" ? utf8(value.toString()) : null;
  },
  decode(bytes) {
    return BigInt(text(bytes));
  },
});

/** Optional codec. Extension 1 preserves arbitrary BigInt values without a size-dependent type. */
export const messagePackCodec: Codec = Object.freeze({
  id: CodecId.MessagePack,
  serialize: (value: unknown) => encode(value, { extensionCodec: extensions }),
  deserialize: (bytes: Uint8Array) =>
    decode(bytes, { useBigInt64: true, extensionCodec: extensions }),
});
