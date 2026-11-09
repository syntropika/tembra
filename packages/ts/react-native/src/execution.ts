import type {
  MobileArtifact,
  MobileBackend,
  MobileExecutionOptions,
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

/** Keep native object identities private to one React Native host. */
export class MobileExecution implements MobileBackend {
  #ready: Promise<void> | undefined;
  readonly #artifacts = new WeakMap<MobileArtifact, string>();
  /** Retain configuration until the first isolated Orb needs a native engine. */
  constructor(
    readonly native: Spec,
    readonly kernel: NativeKernelPort,
    readonly options: MobileExecutionOptions & { engine?: Uint8Array },
  ) {}

  /** Configure the packaged engine and providers once, retrying a failed initialization. */
  private ready(): Promise<void> {
    if (!this.#ready) {
      const pending = this.native.configureExecution(
        this.kernel.handle,
        this.options.engine ? base64(this.options.engine) : "",
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
  async open(artifact: MobileArtifact, bindings: string, grants: string) {
    await this.ready();
    const native = this.native,
      host = this.kernel.handle;
    const id = await native.openPort(host, this.artifactId(artifact), bindings, grants);
    this.logs(host, id);
    return {
      description: () => native.portDescription(host, id),
      begin: (packet: string, declaration: boolean) =>
        native.portBegin(host, id, packet, declaration),
      receive: () => native.portReceive(host, id).finally(() => this.logs(host, id)),
      reply: (reply: number, packet: string) => native.portReply(host, id, reply, packet),
      finish: () => native.portFinish(host, id),
      abort: () => native.abortPort(host, id),
      release: (environment: number) => native.portRelease(host, id, environment),
      io: (packet: string) => native.portIo(host, id, packet),
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
