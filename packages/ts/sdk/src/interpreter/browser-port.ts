import { BrowserIO } from "./browser-io.ts";
import { decodeAsyncHostRequest, decodeHostRequest, type HostRequest } from "./host-protocol.ts";
import type { ExecutionScope, InterpreterPort } from "./types.ts";

type Result = { tag: "ok" | "err"; val: string };
interface Outcome {
  value: Result;
  interrupted: boolean;
}
interface Completion {
  id: bigint;
  value: Result;
}
interface Progress {
  outcome?: Outcome;
  effects: { id: bigint; packet: string }[];
}
export interface BrowserGuest {
  load(packet: string): Outcome | Promise<Outcome>;
  declare(packet: string): Outcome | Promise<Outcome>;
  start(packet: string): Progress | Promise<Progress>;
  resume(completed: Completion[]): Progress | Promise<Progress>;
  cancel(): Outcome | Promise<Outcome>;
}
export interface BrowserGuestModule {
  instantiate(
    core: (path: string) => Promise<WebAssembly.Module>,
    imports: Record<string, unknown>,
  ): BrowserGuest | Promise<BrowserGuest>;
}

/** A serialized Wasm heap yields data; host promises never enter its execution stack. */
export class BrowserPort implements InterpreterPort {
  guest: BrowserGuest | undefined;
  readonly io = new BrowserIO();
  #description = "";
  #scope: ExecutionScope | undefined;
  #running = false;
  #closed = false;
  #poisoned = false;
  /** Retain per-installation I/O and logging independently of callback authority. */
  constructor(
    readonly timeout: number,
    readonly dispose: () => Promise<void>,
    readonly log: (record: Extract<HostRequest, { operation: "console" }>) => void,
    readonly packetBytes = 8 << 20,
  ) {}

  /** UTF-8 uses at most three bytes per UTF-16 code unit; only large packets need encoding. */
  private bounded(packet: string): string {
    if (
      packet.length > this.packetBytes ||
      (packet.length > Math.floor(this.packetBytes / 3) &&
        new TextEncoder().encode(packet).byteLength > this.packetBytes)
    )
      throw new Error("Interpreter packet exceeds its configured bound");
    return packet;
  }

  /** Decode once and invoke the current host-owned scope without an event envelope. */
  sync(packet: string): string {
    try {
      const request = decodeHostRequest(this.bounded(packet));
      if (request.operation === "console") {
        this.log(request);
        return "null";
      }
      if (!this.#scope) throw new Error("No active host scope");
      return this.bounded(JSON.stringify(this.#scope.sync(request) ?? null));
    } catch (error) {
      throw error instanceof Error ? error.message : String(error);
    }
  }

  /** Observe cleanup and interruption in the same export result as the callback. */
  private value(outcome: Outcome): unknown {
    this.#poisoned ||= outcome.interrupted;
    this.bounded(outcome.value.val);
    if (outcome.value.tag === "err") throw new Error(outcome.value.val);
    return JSON.parse(this.bounded(outcome.value.val));
  }

  /** Refuse reuse until the previous stack and its I/O have physically returned. */
  private enter(scope?: ExecutionScope): void {
    if (this.#closed || this.#poisoned || this.#running)
      throw new Error("Interpreter is closed or busy");
    this.#running = true;
    this.#scope = scope;
  }

  /** Initialize one revision before issuing any guest execution scope. */
  async load(packet: string): Promise<void> {
    this.enter();
    try {
      const outcome = await this.guest!.load(this.bounded(packet));
      this.#description = this.bounded(outcome.value.val);
      this.value(outcome);
    } finally {
      this.#running = false;
    }
  }

  /** Read the validated source callback metadata once. */
  description(): string {
    return this.#description;
  }

  /** Factories receive only the declaration scope, even if WASI suspends a native stack. */
  async declare(packet: string, scope: ExecutionScope): Promise<unknown> {
    this.enter(scope);
    try {
      return this.value(await this.guest!.declare(this.bounded(packet)));
    } catch (error) {
      if (error instanceof WebAssembly.RuntimeError) this.#poisoned = true;
      throw error;
    } finally {
      this.#scope = undefined;
      await this.io.settled();
      this.#running = false;
    }
  }

  /** Wait between steps in the host, preserving Promise.race and concurrent effect completion. */
  async execute(packet: string, scope: ExecutionScope): Promise<unknown> {
    this.enter(scope);
    const pending = new Map<bigint, Promise<Completion>>();
    const ready: Completion[] = [];
    let wake: (() => void) | undefined;
    let finished = false;
    let rejectDeadline!: (error: Error) => void;
    const deadline = new Promise<never>((_, reject) => {
      rejectDeadline = reject;
    });
    deadline.catch(() => {});
    const timer = setTimeout(() => {
      this.#poisoned = true;
      scope.cancel();
      rejectDeadline(new Error("Interpreter callback exceeded its wall time limit"));
    }, this.timeout);
    try {
      let progress = await this.guest!.start(this.bounded(packet));
      while (!progress.outcome) {
        if (this.#poisoned) throw new Error("Interpreter callback exceeded its wall time limit");
        for (const effect of progress.effects) {
          if (pending.size >= 128 || pending.has(effect.id))
            throw new Error("Invalid interpreter effect queue");
          const operation = Promise.resolve().then(() =>
            scope.async(decodeAsyncHostRequest(this.bounded(effect.packet))),
          );
          const completed = operation
            .then(
              (value) =>
                ({
                  id: effect.id,
                  value: { tag: "ok", val: this.bounded(JSON.stringify(value ?? null)) },
                }) as Completion,
            )
            .catch(
              (error) =>
                ({ id: effect.id, value: { tag: "err", val: String(error) } }) as Completion,
            )
            .then((response) => {
              ready.push(response);
              wake?.();
              return response;
            });
          pending.set(effect.id, completed);
        }
        if (!ready.length)
          await Promise.race([
            new Promise<void>((resolve) => {
              wake = resolve;
            }),
            deadline,
          ]);
        wake = undefined;
        const completed = ready.splice(0);
        for (const response of completed) pending.delete(response.id);
        progress = await this.guest!.resume(completed);
      }
      finished = true;
      if (this.#poisoned) throw new Error("Interpreter callback exceeded its wall time limit");
      return this.value(progress.outcome);
    } finally {
      clearTimeout(timer);
      scope.cancel();
      if (!finished) {
        this.#poisoned = true;
        try {
          await this.guest!.cancel();
        } catch {
          /* A trapped instance is never reused. */
        }
      }
      this.#scope = undefined;
      await Promise.allSettled(pending.values());
      await this.io.settled();
      this.#running = false;
    }
  }

  /** Pure guest cleanup needs no execution capability after the environment ends. */
  async release(environment: number): Promise<void> {
    if (this.#closed || this.#poisoned) return;
    await this.execute(JSON.stringify({ kind: "release", environment }), {
      check() {},
      cancel() {},
      sync() {
        throw new Error("Release has no host scope");
      },
      async() {
        return Promise.reject(new Error("Release has no host scope"));
      },
    });
  }

  /** Dispose mounted providers only after their last physical operation has settled. */
  async close(): Promise<void> {
    if (this.#running) throw new Error("Guest still owns physical work");
    if (this.#closed) return;
    this.#closed = true;
    this.guest = undefined;
    await this.io.settled();
    await this.dispose();
  }
}
