/**
 * Browser host for the reusable interpreter core module and its private fixed-buffer ABI.
 *
 * Layouts and limits come from `abi.ts`, generated from `crates/executor-wasm/src/abi.rs`.
 * Every Orb receives its own instance, imported memory and JavaScript heap; no Orb code is
 * evaluated in the host realm. Any exception leaving a guest call traps the instance for good.
 */
import * as abi from "./abi.ts";

/** Host-only termination that must trap the instance instead of becoming a catchable reply. */
export class GuestTrap extends Error {}

export interface Outcome {
  ok: boolean;
  value: string;
  interrupted: boolean;
}
/** A JSON host request, or a binary standard WASI packet (first byte `STANDARD_PACKET`). */
export interface Effect {
  id: number;
  packet: string | Uint8Array;
}
/** Successful standard results are bytes; JSON results and every error are text. */
export interface Completion {
  id: number;
  ok: boolean;
  value: string | Uint8Array;
}
export interface Progress {
  outcome?: Outcome;
  effects: Effect[];
}
/** Logical interpreter operations driven by the browser port. */
export interface Guest {
  load(packet: string): Outcome;
  declare(packet: string): Outcome;
  start(packet: string): Progress;
  resume(completed: Completion[]): Progress;
  cancel(): Outcome;
}
/**
 * Host services supplied to one instance; `sync` throws to report an error message.
 * Binary standard packets arrive and reply as bytes; JSON packets stay text.
 */
export interface GuestHost {
  sync(packet: string | Uint8Array): string | Uint8Array;
  log(level: "log" | "error", message: string): void;
  /** Granted per-revision clock; absence exposes a fixed zero to interpreter libc. */
  clock?(clock: number): bigint;
}

const encoder = new TextEncoder();
const decoder = new TextDecoder("utf-8", { fatal: true });

/** Refuse engines whose imports or exports differ from the private ABI before instantiation. */
export function checkInterpreter(module: WebAssembly.Module): void {
  for (const { module: namespace, name, kind } of WebAssembly.Module.imports(module)) {
    const allowed =
      kind === "memory"
        ? namespace === abi.MEMORY_MODULE && name === abi.MEMORY_IMPORT
        : kind === "function" &&
          ((namespace === abi.IMPORT_MODULE &&
            (name === abi.SYNC_IMPORT || name === abi.SYNC_READ_IMPORT)) ||
            (namespace === abi.WASI_MODULE &&
              (abi.WASI_IMPORTS as readonly string[]).includes(name)));
    if (!allowed) throw new Error(`Interpreter import ${namespace}::${name} is not allowed`);
  }
  const exports = new Map(
    WebAssembly.Module.exports(module).map((entry) => [entry.name, entry.kind]),
  );
  for (const name of [abi.INITIALIZE_EXPORT, abi.ALLOC_EXPORT, abi.CALL_EXPORT])
    if (exports.get(name) !== "function") throw new Error(`Interpreter export ${name} is missing`);
}

/** Read one little-endian field from a view created after the latest guest call. */
const field = (view: DataView, offset: number): number => view.getUint32(offset, true);

/** One isolated interpreter instance with a host-bounded linear memory. */
export class CoreGuest implements Guest {
  #trapped = false;
  #reply: Uint8Array | undefined;
  #alloc!: (length: number) => number;
  #call!: (operation: number, pointer: number, length: number) => number;

  private constructor(
    readonly memory: WebAssembly.Memory,
    readonly host: GuestHost,
  ) {}

  /** Instantiate a checked module with a fresh memory whose maximum is the configured bound. */
  static async create(
    module: WebAssembly.Module,
    memoryBytes: number,
    host: GuestHost,
  ): Promise<CoreGuest> {
    const maximum = Math.floor(memoryBytes / abi.PAGE_BYTES);
    if (!Number.isSafeInteger(memoryBytes) || maximum < abi.MEMORY_INITIAL_PAGES || maximum > 65536)
      throw new Error("Invalid interpreter memory bound");
    const memory = new WebAssembly.Memory({ initial: abi.MEMORY_INITIAL_PAGES, maximum });
    const guest = new CoreGuest(memory, host);
    const instance = await WebAssembly.instantiate(module, {
      [abi.MEMORY_MODULE]: { [abi.MEMORY_IMPORT]: memory },
      [abi.IMPORT_MODULE]: {
        [abi.SYNC_IMPORT]: (pointer: number, length: number) => guest.#sync(pointer, length),
        [abi.SYNC_READ_IMPORT]: (pointer: number, length: number) => guest.#read(pointer, length),
      },
      [abi.WASI_MODULE]: wasi(memory, host),
    });
    const exports = instance.exports as Record<string, (...args: number[]) => number>;
    guest.#alloc = exports[abi.ALLOC_EXPORT]!;
    guest.#call = exports[abi.CALL_EXPORT]!;
    guest.#enter(() => exports[abi.INITIALIZE_EXPORT]!());
    return guest;
  }

  /** A trapped instance is never entered again. */
  get trapped(): boolean {
    return this.#trapped;
  }

  /** Check a guest range against the current memory size. */
  #range(pointer: number, length: number): Uint8Array {
    const start = pointer >>> 0;
    const buffer = this.memory.buffer;
    if (start + length > buffer.byteLength) throw new Error("Interpreter range is outside memory");
    return new Uint8Array(buffer, start, length);
  }

  /** Answer one synchronous host request; the reply waits for the matching read. */
  #sync(pointer: number, length: number): number {
    if (this.#reply) throw new Error("Synchronous reply was not read");
    // Invalid ranges or UTF-8 are ABI violations and trap; host failures become messages.
    const range = this.#range(pointer, length >>> 0);
    const binary = range.byteLength > 0 && range[0] === abi.STANDARD_PACKET;
    let ok = true,
      value: string | Uint8Array;
    try {
      // The packet is copied before host code runs; it never aliases guest memory.
      value = binary ? this.host.sync(range.slice()) : this.host.sync(decoder.decode(range));
      if (binary !== value instanceof Uint8Array) throw new Error("Host reply has the wrong kind");
    } catch (error) {
      if (error instanceof GuestTrap) throw error;
      ok = false;
      value = error instanceof Error ? error.message : String(error);
    }
    const bytes = typeof value === "string" ? encoder.encode(value) : value;
    if (bytes.byteLength > abi.LENGTH_LIMIT) throw new Error("Host reply exceeds the ABI limit");
    this.#reply = bytes;
    return (ok ? 0 : abi.SYNC_ERROR) + bytes.byteLength;
  }

  /** Copy the pending reply into the guest's buffer of exactly its length. */
  #read(pointer: number, length: number): void {
    const reply = this.#reply;
    this.#reply = undefined;
    if (!reply || reply.byteLength !== length >>> 0)
      throw new Error("Synchronous reply read does not match");
    this.#range(pointer, reply.byteLength).set(reply);
  }

  /** Mark the instance trapped when any exception leaves the guest. */
  #enter<T>(operation: () => T): T {
    if (this.#trapped) throw new Error("Interpreter instance is trapped");
    try {
      return operation();
    } catch (error) {
      this.#trapped = true;
      throw error;
    }
  }

  /** Copy an input into the guest-reserved buffer, run one operation and return its reply view. */
  #invoke(operation: number, input?: Uint8Array): DataView {
    return this.#enter(() => {
      let pointer = 0,
        length = 0;
      if (input) {
        length = input.byteLength;
        if (length > abi.LENGTH_LIMIT) throw new Error("Interpreter input exceeds the ABI limit");
        pointer = this.#alloc(length) >>> 0;
        if (pointer === 0) throw new Error("Interpreter input reservation was refused");
        // Allocation may grow memory; the range is created from the current buffer.
        this.#range(pointer, length).set(input);
      }
      const reply = this.#call(operation, pointer, length) >>> 0;
      const buffer = this.memory.buffer;
      if (reply + abi.REPLY_HEADER_BYTES > buffer.byteLength)
        throw new Error("Interpreter reply is outside memory");
      const total = new DataView(buffer, reply, 4).getUint32(0, true);
      if (total < abi.REPLY_HEADER_BYTES || reply + total > buffer.byteLength)
        throw new Error("Interpreter reply is outside memory");
      return new DataView(buffer, reply, total);
    });
  }

  /** Decode a reply exactly as `abi::Reply::decode` does; any inconsistency traps. */
  #progress(view: DataView, text = true): Progress & { bytes?: Uint8Array | undefined } {
    return this.#enter(() => {
      const kind = field(view, 4),
        interrupted = field(view, 8),
        count = field(view, 12);
      if (kind > abi.ReplyKind.Error || interrupted > 1)
        throw new Error("Invalid interpreter reply");
      if (count > abi.EFFECT_LIMIT || (kind !== abi.ReplyKind.Pending && count !== 0))
        throw new Error("Invalid interpreter effect count");
      let offset = abi.REPLY_HEADER_BYTES + count * abi.EFFECT_RECORD_BYTES;
      if (offset > view.byteLength) throw new Error("Invalid interpreter reply");
      const body = (length: number): Uint8Array => {
        if (offset + length > view.byteLength) throw new Error("Invalid interpreter reply");
        const bytes = new Uint8Array(view.buffer, view.byteOffset + offset, length);
        offset += length;
        return bytes;
      };
      const value = body(field(view, 16));
      const effects: Effect[] = [];
      for (let index = 0; index < count; index++) {
        const record = abi.REPLY_HEADER_BYTES + index * abi.EFFECT_RECORD_BYTES;
        const low = field(view, record),
          high = field(view, record + 4);
        const id = high * 2 ** 32 + low;
        if (id === 0 || high >= 2 ** 21) throw new Error("Invalid interpreter effect identity");
        const packet = body(field(view, record + 8));
        // Standard packets stay raw bytes; every other effect is UTF-8 JSON.
        effects.push({
          id,
          packet:
            packet[0] === abi.STANDARD_PACKET && packet.byteLength > 0
              ? packet.slice()
              : decoder.decode(packet),
        });
      }
      if (offset !== view.byteLength) throw new Error("Interpreter reply has trailing bytes");
      if (kind === abi.ReplyKind.Pending) return { effects };
      const outcome = {
        ok: kind === abi.ReplyKind.Ok,
        value: text || kind !== abi.ReplyKind.Ok ? decoder.decode(value) : "",
        interrupted: interrupted === 1,
      };
      return { outcome, effects, bytes: text ? undefined : value.slice() };
    });
  }

  /** Operations that must return a final outcome. */
  #outcome(operation: number, input?: string): Outcome {
    const progress = this.#progress(
      this.#invoke(operation, input === undefined ? undefined : encoder.encode(input)),
    );
    if (!progress.outcome) {
      this.#trapped = true;
      throw new Error("Interpreter operation cannot return effects");
    }
    return progress.outcome;
  }

  load(packet: string): Outcome {
    return this.#outcome(abi.Operation.Load, packet);
  }

  declare(packet: string): Outcome {
    return this.#outcome(abi.Operation.Declare, packet);
  }

  start(packet: string): Progress {
    return this.#progress(this.#invoke(abi.Operation.Start, encoder.encode(packet)));
  }

  /** Encode settled effects in one batch; identities stay JavaScript-safe integers. */
  resume(completed: Completion[]): Progress {
    if (!completed.length || completed.length > abi.EFFECT_LIMIT)
      throw new Error("Invalid completion batch");
    const values = completed.map(({ value }) =>
      typeof value === "string" ? encoder.encode(value) : value,
    );
    const records = abi.COMPLETION_HEADER_BYTES + completed.length * abi.COMPLETION_RECORD_BYTES;
    const batch = new Uint8Array(
      values.reduce((total, value) => total + value.byteLength, records),
    );
    const view = new DataView(batch.buffer);
    view.setUint32(0, completed.length, true);
    let offset = records;
    completed.forEach((completion, index) => {
      const { id } = completion;
      if (!Number.isSafeInteger(id) || id < 1) throw new Error("Invalid effect identity");
      const record = abi.COMPLETION_HEADER_BYTES + index * abi.COMPLETION_RECORD_BYTES;
      view.setUint32(record, id % 2 ** 32, true);
      view.setUint32(record + 4, Math.floor(id / 2 ** 32), true);
      view.setUint32(record + 8, completion.ok ? abi.Status.Ok : abi.Status.Error, true);
      view.setUint32(record + 12, values[index]!.byteLength, true);
      batch.set(values[index]!, offset);
      offset += values[index]!.byteLength;
    });
    return this.#progress(this.#invoke(abi.Operation.Resume, batch));
  }

  cancel(): Outcome {
    return this.#outcome(abi.Operation.Cancel);
  }

  /** Capture idle heap bytes; durable continuation restore is a separate host protocol. */
  capture(): Uint8Array {
    const progress = this.#progress(this.#invoke(abi.Operation.Capture), false);
    if (!progress.outcome?.ok) throw new Error(progress.outcome?.value ?? "Capture failed");
    return progress.bytes!;
  }
}

/** Restricted WASI preview 1 functions mirroring `crates/executor-wasm/src/worker/wasi.rs`. */
export function wasi(memory: WebAssembly.Memory, host: GuestHost): Record<string, Function> {
  const open = [true, true, true];
  const isOpen = (descriptor: number): boolean => open[descriptor] === true;
  /** A fresh view per call; memory growth detaches earlier buffers. */
  const view = (): DataView => new DataView(memory.buffer);
  /** Report EFAULT instead of trapping for guest-supplied ranges. */
  const fits = (pointer: number, length: number): boolean =>
    (pointer >>> 0) + length <= memory.buffer.byteLength;
  return {
    clock_time_get(clock: number, _precision: bigint, out: number): number {
      if (clock !== abi.CLOCK_REALTIME && clock !== abi.CLOCK_MONOTONIC) return abi.ERRNO_INVAL;
      if (!fits(out, 8)) return abi.ERRNO_FAULT;
      const nanoseconds = host.clock?.(clock) ?? 0n;
      // WASIp1 has an unsigned nanosecond clock; match the native host's saturating conversion.
      const maximum = (1n << 64n) - 1n;
      view().setBigUint64(
        out >>> 0,
        nanoseconds < 0n ? 0n : nanoseconds > maximum ? maximum : nanoseconds,
        true,
      );
      return abi.ERRNO_SUCCESS;
    },
    environ_get: (): number => abi.ERRNO_SUCCESS,
    environ_sizes_get(count: number, size: number): number {
      if (!fits(count, 4) || !fits(size, 4)) return abi.ERRNO_FAULT;
      view().setUint32(count >>> 0, 0, true);
      view().setUint32(size >>> 0, 0, true);
      return abi.ERRNO_SUCCESS;
    },
    fd_close(descriptor: number): number {
      if (!isOpen(descriptor)) return abi.ERRNO_BADF;
      open[descriptor] = false;
      return abi.ERRNO_SUCCESS;
    },
    fd_fdstat_get(descriptor: number, out: number): number {
      if (!isOpen(descriptor)) return abi.ERRNO_BADF;
      if (!fits(out, abi.FDSTAT_BYTES)) return abi.ERRNO_FAULT;
      new Uint8Array(memory.buffer, out >>> 0, abi.FDSTAT_BYTES).fill(0);
      const stat = view();
      stat.setUint8(out >>> 0, abi.FILETYPE_CHARACTER_DEVICE);
      stat.setUint32((out >>> 0) + 8, descriptor === 0 ? 0 : abi.RIGHT_FD_WRITE, true);
      return abi.ERRNO_SUCCESS;
    },
    fd_prestat_get: (): number => abi.ERRNO_BADF,
    fd_prestat_dir_name: (): number => abi.ERRNO_BADF,
    fd_seek: (descriptor: number): number =>
      isOpen(descriptor) ? abi.ERRNO_SPIPE : abi.ERRNO_BADF,
    path_open: (descriptor: number): number =>
      isOpen(descriptor) ? abi.ERRNO_NOTDIR : abi.ERRNO_BADF,
    fd_write(descriptor: number, vectors: number, count: number, written: number): number {
      const level = descriptor === 1 ? "log" : descriptor === 2 ? "error" : undefined;
      if (!level || !isOpen(descriptor)) return abi.ERRNO_BADF;
      if (!fits(vectors, (count >>> 0) * 8)) return abi.ERRNO_FAULT;
      const table = view();
      const kept = new Uint8Array(abi.STDIO_LIMIT);
      let total = 0,
        size = 0;
      for (let index = 0; index < count >>> 0; index++) {
        const pointer = table.getUint32((vectors >>> 0) + index * 8, true);
        const length = table.getUint32((vectors >>> 0) + index * 8 + 4, true);
        if (!fits(pointer, length)) return abi.ERRNO_FAULT;
        total += length;
        if (total > 0xffffffff) return abi.ERRNO_INVAL;
        const room = Math.min(length, abi.STDIO_LIMIT - size);
        kept.set(new Uint8Array(memory.buffer, pointer >>> 0, room), size);
        size += room;
      }
      if (!fits(written, 4)) return abi.ERRNO_FAULT;
      view().setUint32(written >>> 0, total, true);
      // Diagnostics are lossy text; the write itself is fully consumed.
      const text = new TextDecoder().decode(kept.subarray(0, size)).replace(/\n+$/, "");
      if (text) host.log(level, text);
      return abi.ERRNO_SUCCESS;
    },
    proc_exit(code: number): never {
      throw new Error(`Interpreter exited with status ${code}`);
    },
  };
}
