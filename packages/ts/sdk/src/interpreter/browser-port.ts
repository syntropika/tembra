import { BrowserIO } from "./browser-io.ts";
import type { InterpreterPort } from "./types.ts";

export interface BrowserGuest {
  load(packet: string): Promise<string>;
  declare(packet: string): Promise<string>;
  dispatch(packet: string): Promise<string>;
  interrupt(): Promise<void>;
  hasInterrupted(): Promise<boolean>;
}
export interface BrowserGuestModule {
  instantiate(
    core: (path: string) => Promise<WebAssembly.Module>,
    imports: Record<string, unknown>,
  ): BrowserGuest | Promise<BrowserGuest>;
}

/** Expose the same private transport as Node while retaining the browser component's actual task. */
export class BrowserPort implements InterpreterPort {
  guest: BrowserGuest | undefined;
  readonly io = new BrowserIO();
  #description = "";
  #callback: ((packet: string) => string) | undefined;
  #event: ((packet: string) => void) | undefined;
  #replies = new Map<number, { resolve(value: string): void; reject(error: unknown): void }>();
  #next = 0;
  #running = false;
  #done = false;
  #closed = false;
  #poisoned = false;
  #epoch = 0;
  #ticket = 0;
  #values = new Map<number, { value?: string; error?: unknown }>();
  /** Keep callback response data separate from canonical ABI allocation. */
  constructor(
    readonly timeout: number,
    readonly dispose: () => Promise<void>,
    readonly log: (packet: string) => void,
    readonly packetBytes = 8 << 20,
  ) {}

  /** Scalar ticket results avoid allocating strings or abandoned resources during async return. */
  /** Bound UTF-8 payloads before parsing or copying them back into linear memory. */
  private bounded(packet: string): string {
    if (
      packet.length > this.packetBytes ||
      new TextEncoder().encode(packet).byteLength > this.packetBytes
    )
      throw new Error("Interpreter packet exceeds its configured bound");
    return packet;
  }
  take(ticket: number): string {
    const value = this.#values.get(ticket);
    this.#values.delete(ticket);
    if (!value) throw "unknown or expired response ticket";
    if (value.error !== undefined) throw value.error;
    this.bounded(value.value!);
    return value.value!;
  }
  /** Resolve synchronous requests only through the currently installed host scope. */
  sync(packet: string): string {
    this.bounded(packet);
    if (JSON.parse(packet).operation === "console") {
      this.log(packet);
      return "null";
    }
    if (this.#callback) {
      const result = JSON.parse(this.bounded(this.#callback(packet)));
      if (Object.hasOwn(result, "ok")) return JSON.stringify(result.ok);
      throw result.error;
    }
    if (!this.#event) throw new Error("No active host scope");
    // Synchronous WASI/JS calls cannot suspend to a later browser turn.
    let reply: string | undefined;
    let failure: unknown;
    const id = ++this.#next;
    this.#replies.set(id, {
      resolve(value) {
        reply = value;
      },
      reject(error) {
        failure = error;
      },
    });
    this.#event(JSON.stringify({ event: "host", id, asynchronous: false, packet }));
    if (failure !== undefined) throw failure;
    if (reply === undefined) {
      this.#replies.delete(id);
      throw new Error("Synchronous host operation did not settle");
    }
    return reply;
  }
  /** Late results are discarded by epoch before they can allocate retained reply data. */
  async async(packet: string): Promise<number> {
    const epoch = this.#epoch;
    let result: { value?: string; error?: unknown };
    try {
      this.bounded(packet);
      if (!this.#event) throw new Error("No active host scope");
      if (this.#replies.size >= 128) throw new Error("Interpreter host operation queue is full");
      const value = await new Promise<string>((resolve, reject) => {
        const id = ++this.#next;
        this.#replies.set(id, { resolve, reject });
        this.#event!(JSON.stringify({ event: "host", id, asynchronous: true, packet }));
      });
      result = { value };
    } catch (error) {
      result = { error: error instanceof Error ? error.message : String(error) };
    }
    if (epoch !== this.#epoch || !this.#event) return 0;
    if (this.#ticket === 0xffffffff) throw new Error("Response ticket space exhausted");
    const ticket = ++this.#ticket;
    this.#values.set(ticket, result);
    return ticket;
  }
  /** Load one source revision before exposing it to a lifecycle kernel. */
  async load(packet: string): Promise<void> {
    this.#description = this.bounded(await this.guest!.load(this.bounded(packet)));
  }
  /** Guest-map cleanup needs no renewed authority after the kernel drops an environment. */
  async release(environment: number): Promise<void> {
    if (this.#closed || this.#poisoned) return;
    if (this.#running) throw new Error("Interpreter is busy");
    this.#running = true;
    try {
      await this.guest!.dispatch(JSON.stringify({ kind: "release", environment }));
      await this.io.settled();
    } finally {
      this.#running = false;
    }
  }
  /** Read validated callback metadata. */
  description(): string {
    return this.#description;
  }
  /** Preserve factory synchrony without giving guest code access to ambient host globals. */
  async declare(packet: string, callback: (packet: string) => string): Promise<string> {
    if (this.#closed || this.#poisoned || this.#running)
      throw new Error("Interpreter is closed or busy");
    this.#running = true;
    this.#callback = callback;
    try {
      return JSON.stringify({
        ok: JSON.parse(this.bounded(await this.guest!.declare(this.bounded(packet)))),
      });
    } catch (error) {
      return JSON.stringify({ error: String(error) });
    } finally {
      this.#callback = undefined;
      try {
        this.#poisoned ||= await this.guest!.hasInterrupted();
      } catch {
        this.#poisoned = true;
      }
      this.#running = false;
    }
  }
  /** Physical completion, rather than Promise.race, determines when this instance can be reused. */
  dispatch(packet: string, callback: (packet: string) => void): void {
    if (this.#closed || this.#poisoned || this.#running)
      throw new Error("Interpreter is closed or busy");
    this.#running = true;
    this.#epoch++;
    this.#done = false;
    this.#event = callback;
    const timer = setTimeout(() => {
      this.#poisoned = true;
      this.guest!.interrupt().catch(() => {
        /* The original exported task remains the settlement authority. */
      });
    }, this.timeout);
    Promise.resolve()
      .then(() => this.guest!.dispatch(this.bounded(packet)))
      .then(
        (value) => {
          try {
            return { ok: JSON.parse(this.bounded(value)) };
          } catch (error) {
            return { error: String(error) };
          }
        },
        (error) => {
          if (error instanceof WebAssembly.RuntimeError) this.#poisoned = true;
          return { error: String(error) };
        },
      )
      .then(async (result) => {
        clearTimeout(timer);
        if (!this.#poisoned) {
          try {
            this.#poisoned = await this.guest!.hasInterrupted();
          } catch {
            this.#poisoned = true;
          }
        }
        await this.io.settled();
        this.#done = true;
        this.#event = undefined;
        for (const reply of this.#replies.values()) reply.reject(new Error("Callback scope ended"));
        this.#replies.clear();
        this.#values.clear();
        callback(JSON.stringify({ event: "complete", result }));
      });
  }
  /** Late replies cannot resolve a request in a later exchange. */
  reply(id: number, packet: string): boolean {
    const pending = this.#replies.get(id);
    if (!pending) return false;
    this.#replies.delete(id);
    try {
      const value = JSON.parse(this.bounded(packet));
      if (Object.hasOwn(value, "ok")) pending.resolve(JSON.stringify(value.ok));
      else pending.reject(value.error);
    } catch (error) {
      pending.reject(error instanceof Error ? error.message : String(error));
    }
    return true;
  }
  /** Reopen the local ticket only after the component's exported task returned. */
  finish(): void {
    if (!this.#done) throw new Error("Guest has not physically completed");
    this.#running = false;
  }
  /** Disposal is awaited separately by the revision's lifecycle hook. */
  async close(): Promise<void> {
    if (this.#running) throw new Error("Guest still owns physical work");
    if (this.#closed) return;
    this.#closed = true;
    this.guest = undefined;
    await this.io.settled();
    await this.dispose();
  }
}
