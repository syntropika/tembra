// Derived from componentize-qjs v0.6.0 (commit 758687536ea711ede17369f53a90ddc3e97a8dbb),
// crates/core/src/js/stream-from.js, Apache License 2.0. Tembra change, matching
// crates/executor-wasm/guest/patches/runtime.patch: a producer failure calls the runtime's
// native `__cqjs.failStream`, which traps the instance before the writer is dropped, so a
// reader can never observe a failed producer as a clean end of stream.
wit.Stream.from = function (iterable, type) {
  const { readable, writable } = wit.Stream(type);
  const completion = (async () => {
    try {
      for await (const item of iterable) {
        if (!(await writable.writeIterableItem(item))) break;
      }
    } catch (error) {
      // Trap before dropping the writer: failure must never appear as clean EOF.
      __cqjs.failStream(error);
      throw error;
    } finally {
      writable.drop();
    }
  })();
  return { readable, completion };
};
