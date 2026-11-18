// All continuation state stays in the JavaScript heap; Rust owns only the surrounding job pump.
(() => {
  const active = globalThis.__scopeActive!;
  delete globalThis.__scopeActive;
  const pending = new Map<
    number,
    { packet: string | Uint8Array; resolve(value: any): void; reject(error: Error): void }
  >();
  const outbox: { id: number; packet: string | Uint8Array }[] = [];
  /** Waiters for the moment no host operation of this callback is pending. */
  let idle: (() => void)[] = [];
  let sequence = 0;
  /**
   * Each refusal of the host this heap received, by the identity the executor knows it by: the
   * effect that delivered it, or the synchronous request it refused. A callback that fails with
   * one unchanged names it, and the executor answers that refusal. A guest can name only a
   * refusal the host sent it in that callback.
   */
  const refusals = new WeakMap<object, { effect: number } | { request: number }>();
  const hostSync = globalThis.__hostSync;
  globalThis.__hostSync = (packet: string) => {
    try {
      return hostSync(packet);
    } catch (error) {
      if (typeof error === "object" && error !== null) {
        try {
          // Asked at once: the executor answers about the call that just failed.
          const request: unknown = JSON.parse(hostSync('{"operation":"refusal"}'));
          if (typeof request === "number") refusals.set(error, { request });
        } catch {
          // A host that keeps no refusals names none.
        }
      }
      throw error;
    }
  };
  globalThis.__tembraNameRefusal = (error: unknown) => {
    const refusal = typeof error === "object" && error !== null ? refusals.get(error) : undefined;
    if (!refusal) return;
    try {
      hostSync(JSON.stringify({ operation: "refused", ...refusal }));
    } catch {
      // Naming never replaces the callback's own failure.
    }
  };
  function settle(): void {
    if (pending.size) return;
    const waiters = idle;
    idle = [];
    for (const wake of waiters) wake();
  }
  globalThis.__hostAsync = ((packet: string | Uint8Array) => {
    if (!active()) throw new Error("callback scope is closed");
    if (pending.size >= 128) throw new Error("host operation queue is full");
    if (!Number.isSafeInteger(sequence + 1)) throw new Error("effect identity space exhausted");
    const id = ++sequence;
    return new Promise((resolve, reject) => {
      pending.set(id, { packet, resolve, reject });
      outbox.push({ id, packet });
    });
  }) as typeof globalThis.__hostAsync;
  globalThis.__tembraTakeEffects = () => outbox.splice(0);
  globalThis.__tembraCompleteEffect = (id, ok, value) => {
    const operation = pending.get(id);
    if (!operation) throw new Error("unknown or completed effect");
    pending.delete(id);
    if (ok) operation.resolve(value);
    else {
      const error = new Error(String(value));
      refusals.set(error, { effect: id });
      operation.reject(error);
    }
    settle();
  };
  globalThis.__tembraStopEffects = () => {
    for (const operation of pending.values()) operation.reject(new Error("callback scope ended"));
    pending.clear();
    outbox.length = 0;
    settle();
    delete globalThis.__tembraPendingDispatch;
    globalThis.__tembraAbandon?.();
  };
  // Read by the incoming HTTP callback to end only once its own host work is finished.
  globalThis.__tembraEffects = Object.freeze({
    get pending() {
      return pending.size;
    },
    get sequence() {
      return sequence;
    },
    idle(): Promise<void> {
      return pending.size ? new Promise<void>((resolve) => idle.push(resolve)) : Promise.resolve();
    },
  });
})();
