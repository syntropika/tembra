// All continuation state stays in the JavaScript heap; Rust owns only the surrounding job pump.
(() => {
  const active = globalThis.__scopeActive!;
  delete globalThis.__scopeActive;
  const pending = new Map<
    number,
    { packet: string; resolve(value: string): void; reject(error: Error): void }
  >();
  const outbox: { id: number; packet: string }[] = [];
  let sequence = 0;
  globalThis.__hostAsync = (packet) => {
    if (!active()) return Promise.reject(new Error("callback scope is closed"));
    if (pending.size >= 128) return Promise.reject(new Error("host operation queue is full"));
    if (!Number.isSafeInteger(sequence + 1)) throw new Error("effect identity space exhausted");
    const id = ++sequence;
    return new Promise((resolve, reject) => {
      pending.set(id, { packet, resolve, reject });
      outbox.push({ id, packet });
    });
  };
  globalThis.__tembraTakeEffects = () => outbox.splice(0);
  globalThis.__tembraCompleteEffect = (id, ok, value) => {
    const operation = pending.get(id);
    if (!operation) throw new Error("unknown or completed effect");
    pending.delete(id);
    if (ok) operation.resolve(value);
    else operation.reject(new Error(value));
  };
  globalThis.__tembraStopEffects = () => {
    for (const operation of pending.values()) operation.reject(new Error("callback scope ended"));
    pending.clear();
    outbox.length = 0;
    delete globalThis.__tembraPendingDispatch;
    globalThis.__tembraAbandon?.();
  };
})();
