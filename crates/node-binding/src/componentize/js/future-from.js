// Copied unchanged from componentize-qjs v0.6.0 (commit 758687536ea711ede17369f53a90ddc3e97a8dbb),
// crates/core/src/js/future-from.js, Apache License 2.0. Futures appear in WASI 0.3 import
// signatures (filesystem, sockets, HTTP); the generic lowering stays upstream's.
wit.Future.from = function (value, type) {
  const { readable, writable } = wit.Future(type);
  const completion = (async () => {
    try {
      await writable.write(await value);
    } finally {
      writable.drop();
    }
  })();
  return { readable, completion };
};
