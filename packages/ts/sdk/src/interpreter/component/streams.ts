/**
 * Private, single-owner transport for the canonical whole-result `stream<u8>` profile.
 * A published stream is a capability, never JSON or a JCO representation shared between
 * instances. The producer's physical completion remains independent of publication and drop.
 */
import type { Context } from "../../runtime/context.ts";
import type { ByteStreamCapability } from "../../runtime/adapters.ts";
import { abortable, guestBytes, hostBytes, STREAM_CHUNK } from "./wasi-host.ts";

const sources = new WeakMap<ByteStreamCapability, Source>();

/** The receiving call may retain this capability only for its original root. */
export class ByteStreamOwner implements ByteStreamCapability {
  readonly kind = "bytes" as const;

  private constructor(
    readonly finished: Promise<void>,
    source: Source,
  ) {
    sources.set(this, source);
  }

  /** Issue only at the checked whole-result stream seam. */
  static issue(stream: unknown, finished: Promise<void>, signal: AbortSignal): ByteStreamOwner {
    return new ByteStreamOwner(finished, new Source(stream, finished, signal));
  }

  /** Anchor publication to the admitted receiver; cancellation promptly drops the reader. */
  bind(context: Context): void {
    const source = claim(this);
    context.check();
    if (source.binding) {
      if (source.binding.session !== context.session || source.binding.root !== context.root)
        throw new Error("Byte stream belongs to another root");
      return;
    }
    source.binding = { session: context.session, root: context.root };
    source.attach(context.signal);
  }

  /** Request reader drop immediately. `finished` separately acknowledges physical completion. */
  close(): void {
    claim(this).close();
  }

  /** Publication and receipt share one host report even if the receiver never takes its head. */
  reportFailure(error: unknown, report: (error: unknown) => void): void {
    claim(this).reportFailure(error, report);
  }
}

export const issueByteStream = ByteStreamOwner.issue;

function claim(owner: ByteStreamCapability): Source {
  const source = sources.get(owner);
  if (!source) throw new TypeError("Foreign byte stream capability");
  return source;
}

/** One bounded demand source. Queued readers never overlap a JCO stream read. */
class Source {
  readonly #iterator: AsyncIterableIterator<Uint8Array>;
  readonly #stopped = new AbortController();
  readonly #detach = new Set<() => void>();
  readonly #failed: Promise<never>;
  #queue = Promise.resolve();
  #closed = false;
  #failure: { error: unknown } | undefined;
  #reported = false;
  taken = false;
  binding: { session: object; root: number } | undefined;

  constructor(
    stream: unknown,
    readonly finished: Promise<void>,
    signal: AbortSignal,
  ) {
    this.#iterator = guestBytes(stream, this.#stopped.signal);
    this.#failed = finished.then(
      () => new Promise<never>(() => {}),
      (error: unknown) => {
        this.#failure = { error };
        this.close();
        throw error;
      },
    );
    this.#failed.catch(() => {});
    this.attach(signal);
  }

  attach(signal: AbortSignal): void {
    if (this.#closed) return;
    if (signal.aborted) return this.close();
    const close = () => this.close();
    signal.addEventListener("abort", close, { once: true });
    this.#detach.add(() => signal.removeEventListener("abort", close));
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    this.#stopped.abort();
    for (const detach of this.#detach) detach();
    this.#detach.clear();
    // guestBytes.return drops the raw JCO stream synchronously, including during a quiet read.
    void this.#iterator.return?.().catch(() => {});
  }

  #checkFailure(): void {
    if (this.#failure) throw this.#failure.error;
  }

  reportFailure(error: unknown, report: (error: unknown) => void): void {
    if (this.#reported) return;
    this.#reported = true;
    report(error);
  }

  read(): Promise<Uint8Array | undefined> {
    const next = this.#queue.then(async () => {
      this.#checkFailure();
      if (this.#closed) return undefined;
      try {
        const result = await Promise.race([this.#iterator.next(), this.#failed]);
        this.#checkFailure();
        if (this.#closed) return undefined;
        if (result.done) {
          // Writer EOF alone is insufficient: a task can trap after publishing its last byte.
          await abortable(this.finished, this.#stopped.signal);
          this.close();
          return undefined;
        }
        if (result.value.byteLength > STREAM_CHUNK)
          throw new Error("Byte stream chunk exceeds the 64 KiB bound");
        // No producer memory or mutable view crosses into another component instance.
        return result.value.slice();
      } catch (error) {
        const dropped = this.#closed;
        this.close();
        this.#checkFailure();
        if (dropped) return undefined;
        throw error;
      }
    });
    this.#queue = next.then(
      () => {},
      () => {},
    );
    return next;
  }
}

/**
 * Redeem once as a symbol-free iterator accepted by the receiving JCO glue. Plain byte streams
 * have no guest error lane: report the producer cause to the host and end the guest stream.
 * Cleanup retains the physical producer after early drop without blocking drop acknowledgement.
 */
export function receiveByteStream(
  owner: ByteStreamCapability,
  ended: AbortSignal,
  cleanup: (operation: () => unknown) => void,
  failed: (error: unknown) => void,
): AsyncIterableIterator<Uint8Array> {
  const source = claim(owner);
  if (source.taken) throw new Error("Byte stream capability was already consumed");
  source.taken = true;
  source.attach(ended);
  const report = (error: unknown) => {
    owner.reportFailure(error, failed);
  };
  return hostBytes(
    () => source.read(),
    (end, failure) => {
      owner.close();
      if (end === "failed") report(failure);
      cleanup(() => owner.finished.catch(report));
    },
    ended,
  );
}
