import type {
  MobileArtifact,
  MobileBackend,
  MobileExecutionOptions,
  MobileField,
  MobileIngressCarrier,
  MobileIngressEvent,
} from "@syntropika/tembra-sdk/runtime";
import type { Spec } from "./NativeTembra.ts";
import type { NativeKernelPort } from "./kernel.ts";

/** Encode bytes for the TurboModule without depending on Node buffers or browser globals. */
function base64(bytes: Uint8Array): string {
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const chunks: string[] = [];
  for (let start = 0; start < bytes.length; start += 12288) {
    let value = "";
    for (let i = start; i < Math.min(start + 12288, bytes.length); i += 3) {
      const a = bytes[i]!,
        b = bytes[i + 1],
        c = bytes[i + 2];
      value +=
        alphabet[a >> 2]! +
        alphabet[((a & 3) << 4) | ((b ?? 0) >> 4)]! +
        (b === undefined ? "=" : alphabet[((b & 15) << 2) | ((c ?? 0) >> 6)]!) +
        (c === undefined ? "=" : alphabet[c & 63]!);
    }
    chunks.push(value);
  }
  return chunks.join("");
}

/** The exact bytes of a view as an `ArrayBuffer` the TurboModule can carry, copying only a partial view. */
function exact(bytes: Uint8Array): ArrayBuffer {
  return bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength
    ? (bytes.buffer as ArrayBuffer)
    : (bytes.slice().buffer as ArrayBuffer);
}

/** Field names and lengths for metadata; the values concatenated, in order, into one buffer. */
function packFields(fields: readonly MobileField[]): {
  fields: [string, number][];
  bytes: ArrayBuffer;
} {
  let length = 0;
  for (const field of fields) length += field.value.byteLength;
  const bytes = new Uint8Array(length);
  let offset = 0;
  for (const field of fields) {
    bytes.set(field.value, offset);
    offset += field.value.byteLength;
  }
  return {
    fields: fields.map((field) => [field.name, field.value.byteLength]),
    bytes: bytes.buffer,
  };
}

/** Split a taken buffer back into ordered fields, refusing any metadata that does not cover it. */
function unpackFields(fields: unknown, buffer: ArrayBuffer): MobileField[] {
  if (!Array.isArray(fields)) throw new TypeError("Invalid native ingress field metadata");
  let offset = 0;
  const unpacked = fields.map((field: unknown) => {
    if (!Array.isArray(field) || field.length !== 2)
      throw new TypeError("Invalid native ingress field");
    const [name, length] = field as [unknown, unknown];
    if (typeof name !== "string" || !Number.isSafeInteger(length) || (length as number) < 0)
      throw new TypeError("Invalid native ingress field");
    if (offset + (length as number) > buffer.byteLength)
      throw new TypeError("Native ingress field lengths exceed their bytes");
    const value = new Uint8Array(buffer, offset, length as number);
    offset += length as number;
    return { name, value };
  });
  if (offset !== buffer.byteLength)
    throw new TypeError("Native ingress field lengths do not cover their bytes");
  return unpacked;
}

/** The bridge methods of the typed carrier; the group is offered only when all are present. */
const INGRESS = [
  "portBeginIngress",
  "portReceiveIngress",
  "portTakeIngress",
  "portSupplyRequest",
  "portPullResponse",
  "portDisconnect",
] as const;

/**
 * Adapt the native port's incoming HTTP carrier. A byte event's metadata is followed in the same
 * turn by the synchronous take of its one pending buffer, so events are consumed strictly in order.
 */
function carrier(native: Spec, host: string, id: string, logs: () => void): MobileIngressCarrier {
  const empty = new ArrayBuffer(0);
  return {
    beginIngress(packet, head) {
      const fields = packFields(head.fields);
      const metadata = JSON.stringify({
        method: head.method,
        scheme: head.scheme,
        authority: head.authority,
        pathWithQuery: head.pathWithQuery,
        fields: fields.fields,
      });
      native.portBeginIngress(host, id, packet, metadata, fields.bytes);
    },
    async receiveIngress(): Promise<MobileIngressEvent> {
      let text: string;
      try {
        text = await native.portReceiveIngress(host, id);
      } finally {
        logs();
      }
      const event = JSON.parse(text) as Record<string, unknown> & { kind?: unknown };
      switch (event.kind) {
        case "head":
        case "trailers":
        case "data":
        case "refused": {
          const bytes = native.portTakeIngress(host, id);
          if (!(bytes instanceof ArrayBuffer))
            throw new TypeError("Native ingress bytes are not an ArrayBuffer");
          if (event.kind === "data" || event.kind === "refused") {
            if (event.length !== bytes.byteLength)
              throw new TypeError("Native ingress byte length mismatch");
            return event.kind === "data"
              ? { kind: "data", bytes: new Uint8Array(bytes) }
              : { kind: "refused", code: new Uint8Array(bytes) };
          }
          const fields = unpackFields(event.fields, bytes);
          if (event.kind === "trailers") return { kind: "trailers", fields };
          if (!Number.isInteger(event.status)) throw new TypeError("Invalid native ingress status");
          return { kind: "head", status: event.status as number, fields };
        }
        case "host":
          return {
            kind: "host",
            id: event.id as number,
            asynchronous: event.asynchronous as boolean,
            packet: event.packet as string,
          };
        case "complete":
          return { kind: "complete", result: event.result as string };
        case "requestPull":
          return { kind: "requestPull" };
        case "end":
          return { kind: "end" };
        case "error":
          return { kind: "error", message: event.message as string };
        default:
          throw new TypeError("Unknown native ingress event");
      }
    },
    supplyRequest(chunk) {
      if (chunk.kind === "data")
        return native.portSupplyRequest(host, id, '{"kind":"data"}', exact(chunk.bytes));
      if (chunk.kind === "end") {
        const trailers = packFields(chunk.trailers);
        return native.portSupplyRequest(
          host,
          id,
          JSON.stringify({ kind: "end", trailers: trailers.fields }),
          trailers.bytes,
        );
      }
      return native.portSupplyRequest(
        host,
        id,
        JSON.stringify({ kind: "error", message: chunk.message }),
        empty,
      );
    },
    pullResponse() {
      native.portPullResponse(host, id);
    },
    disconnect() {
      native.portDisconnect(host, id);
    },
  };
}

/** Keep native object identities private to one React Native host. */
export class MobileExecution implements MobileBackend {
  #ready: Promise<void> | undefined;
  readonly #artifacts = new WeakMap<MobileArtifact, string>();
  /** Retain configuration until the first isolated Orb needs a native engine. */
  constructor(
    readonly native: Spec,
    readonly kernel: NativeKernelPort,
    readonly options: MobileExecutionOptions,
  ) {}

  /** Configure portable execution and providers once, retrying a failed initialization. */
  private ready(): Promise<void> {
    if (!this.#ready) {
      const pending = this.native.configureExecution(
        this.kernel.handle,
        JSON.stringify({
          limits: this.options.limits ?? {},
          filesystem: Object.fromEntries(
            Object.entries(this.options.providers?.filesystem ?? {}).map(([name, provider]) => [
              name,
              provider.path,
            ]),
          ),
          network: this.options.providers?.network ?? [],
        }),
      );
      this.#ready = pending;
      void pending.catch(() => {
        if (this.#ready === pending) this.#ready = undefined;
      });
    }
    return this.#ready;
  }

  /** Decode bytes into a native prepared contract without evaluating host JavaScript. */
  artifact(bytes: Uint8Array): MobileArtifact {
    const native = this.native,
      host = this.kernel.handle;
    const id = native.createArtifact(host, base64(bytes));
    let closed = false;
    const artifact: MobileArtifact = {
      description: () => native.artifactDescription(host, id),
      request: (packet) => native.artifactRequest(host, id, packet),
      bind: (binding, method, exports) => native.artifactBind(host, id, binding, method, exports),
      validate: (slot, arguments_, value) =>
        native.artifactValidate(host, id, slot, arguments_, value),
      free: () => {
        if (!closed) {
          closed = true;
          native.releaseArtifact(host, id);
        }
      },
    };
    this.#artifacts.set(artifact, id);
    return artifact;
  }

  /** Open source or compiled execution while the shared SDK retains lifecycle and callback authority. */
  async open(artifact: MobileArtifact, bindings: string, grants: string, cli: string) {
    await this.ready();
    const native = this.native,
      host = this.kernel.handle;
    const id = await native.openPort(host, this.artifactId(artifact), bindings, grants, cli);
    this.logs(host, id);
    const wired = INGRESS.every((name) => typeof native[name] === "function");
    return {
      ingress: wired ? carrier(native, host, id, () => this.logs(host, id)) : undefined,
      description: () => native.portDescription(host, id),
      begin: (packet: string, declaration: boolean) =>
        native.portBegin(host, id, packet, declaration),
      receive: () => native.portReceive(host, id).finally(() => this.logs(host, id)),
      reply: (reply: number, packet: string) => native.portReply(host, id, reply, packet),
      finish: () => native.portFinish(host, id),
      abort: () => native.abortPort(host, id),
      release: (environment: number) => native.portRelease(host, id, environment),
      cancel: () => {
        native.portCancel(host, id);
      },
      close: () => native.closePort(host, id),
    };
  }

  /** Diagnostics cannot throw into guest work or retain an unbounded native queue. */
  private logs(host: string, port: string): void {
    try {
      const records = JSON.parse(this.native.portLogs(host, port)) as {
        orb: string;
        level: string;
        message: string;
      }[];
      for (const record of records) {
        try {
          this.options.onLog?.(record);
        } catch {
          /* Observers do not control the guest. */
        }
      }
    } catch {
      /* Host invalidation may have already released the port. */
    }
  }

  /** Reject foreign prepared artifacts before crossing the native boundary. */
  private artifactId(artifact: MobileArtifact): string {
    const id = this.#artifacts.get(artifact);
    if (!id) throw new Error("Foreign mobile artifact");
    return id;
  }
}
