(async () => {
  const Scope = Tembra.ContextKey.opaque("storage/pending");
  const Trace = Tembra.ContextKey.text("bench/trace");
  const store = await host.get("storage", "main");
  const check = (value, message) => {
    if (!value) throw new Error(message);
  };
  const rejects = async (operation, reason) => {
    let error;
    try {
      await operation();
    } catch (failure) {
      error = failure;
    }
    check(
      error && (!reason || error.reason === reason),
      `Expected refusal ${reason}, got ${error}`,
    );
  };
  const cases = [];
  await host.root(async (root) => {
    const frame = await store.open(root);
    const pending = root.with(Scope, Tembra.opaqueFrom(root, Scope, frame));
    const left = pending.with(Trace, "left");
    const right = pending.with(Trace, "right");
    check(
      JSON.stringify(await Promise.all([probe.trace(left), probe.trace(right)])) ===
        JSON.stringify(["left", "right"]),
      "Rust received a shared root value instead of per-call branches",
    );
    await Promise.all([store.append(1, left), store.append(2, right)]);
    const before = await store.stats(root);
    check(
      before.rows.length === 0 && before.backendTransactionsOpen === 0,
      "Backend transaction held across calls",
    );
    await rejects(() => store.append(3, root), "storage_scope");
    await store.close(true, pending);
    await rejects(() => store.append(3, pending), "storage_scope_closed");
    const after = await store.stats(root);
    check(after.rows.length === 2 && after.commits === 1, "Pending writes not committed together");
    check(left.value(Trace) === "left" && right.value(Trace) === "right", "Siblings leaked values");
    cases.push("pending writes, concurrent branches, commit and closed-scope refusal");
    const lateFrame = await store.open(root);
    const late = root.with(Scope, Tembra.opaqueFrom(root, Scope, lateFrame));
    const result = store.appendDelayed(9, late).catch((error) => error);
    while (!(await store.stats(root)).delayed)
      await new Promise((resolve) => setTimeout(resolve, 0));
    await store.close(false, late);
    check(host.diagnostics().calls > 0, "Logical close hid physical work");
    const error = await result;
    check(error.reason === "storage_scope_closed", "Delayed write reused closed scope");
    check((await store.stats(root)).rows.length === 2, "Aborted delayed write reached backend");
    cases.push("close during physical work prevents late writes");
    await rejects(() => Promise.resolve(root.with(Scope, { opaque: true })), undefined);
    await rejects(() => Promise.resolve(root.exportValues([Scope])), undefined);
    cases.push("forged token and implicit opaque export refused");
  });
  check(
    host.session.kernel.command("context-stats").live === 0,
    "Frames retained after root settlement",
  );
  await host.root(async (root) => {
    const frame = await store.open(root);
    const scoped = root.with(Scope, Tembra.opaqueFrom(root, Scope, frame));
    const result = store.appendDelayed(10, scoped).catch((error) => error);
    while (!(await store.stats(root)).delayed)
      await new Promise((resolve) => setTimeout(resolve, 0));
    root.cancel();
    check(host.diagnostics().calls > 0, "Cancellation pretended work had settled");
    const error = await result;
    check(error.code === "cancelled", "Cancelled delayed operation wrote state");
    cases.push("cancellation preserves physical lifetime and blocks late writes");
  });
  check(
    host.session.kernel.command("context-stats").live > 0,
    "Cancellation released physical context too early",
  );
  while (host.diagnostics().calls > 0) await new Promise((resolve) => setTimeout(resolve, 0));
  check(
    host.session.kernel.command("context-stats").live === 0,
    "Cancelled-root frames leaked after physical settlement",
  );
  check((await store.stats()).pendingScopes === 0, "Storage scopes retained after root settlement");
  store.release();
  return cases;
})();
