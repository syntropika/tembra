/**
 * Exact, fail-closed instrumentation of JCO 0.23.0 glue, applied at build time by Tembra tooling
 * to bindings generated from natively verified component bytes; browsers never run this code.
 * It adds per-instance export-task start, task exit (with rejection) and trap notifications plus
 * a current-root-task accessor through the official 4th `instantiate(..., options)` argument.
 * Every anchor must occur exactly once; any vendor drift refuses the glue instead of emitting
 * uninstrumented code.
 */
export const JCO_VERSION = "0.23.0";

const HEADER =
  '"use components";\nexport function instantiate(getCoreModule, imports, instantiateCore, options) {\n';
const TASK_ANCHOR = "      const newTaskMeta = { id: newTaskID, componentIdx, task: newTask };\n";
const TRAP_ANCHOR =
  "        this.#trapped = true;\n        if (this.#trapError === null) { this.#trapError = err; }\n";

const PRELUDE = `  const __tembraObserver = options?.tembraObserver ?? null;
  function __tembraObserveTask(task, args) {
    if (__tembraObserver === null || !args.callingWasmExport) { return; }
    const id = task.id();
    __tembraObserver.taskStarted({ id, entry: args.entryFnName, isAsync: !!args.isAsync });
    task.exitPromise().then(
      () => __tembraObserver.taskExited({ id, rejected: task.isRejected(), error: task.isErrored() ?? undefined }),
      (error) => __tembraObserver.taskExited({ id, error }),
    );
  }
  function __tembraObserveTrap(error) {
    if (__tembraObserver !== null) { __tembraObserver.trapped(error); }
  }
  if (__tembraObserver !== null) {
    __tembraObserver.attach({
      /** Root export task of the guest code running now (call synchronously at host-import entry). */
      currentRootTask() {
        const meta = CURRENT_TASK_META.current;
        if (!meta || meta.taskID === undefined || meta.componentIdx === undefined) { return null; }
        const task = getCurrentTask(meta.componentIdx, meta.taskID)?.task;
        return task ? task.getRootTask().id() : null;
      },
    });
  }
`;

function once(source: string, anchor: string, label: string): number {
  const first = source.indexOf(anchor);
  if (first < 0 || source.indexOf(anchor, first + 1) >= 0) {
    throw new Error(
      `unsupported trusted compiler output: anchor "${label}" must occur exactly once (expected JCO ${JCO_VERSION})`,
    );
  }
  return first;
}

/** Instrument generated glue through its task and trap notifications. */
export function instrumentGlue(source: string): string {
  if (!source.startsWith(HEADER)) {
    throw new Error(
      `unsupported trusted compiler output: instantiate signature (expected JCO ${JCO_VERSION})`,
    );
  }
  once(source, TASK_ANCHOR, "task creation");
  once(source, TRAP_ANCHOR, "trap record");
  once(source, "    const CURRENT_TASK_META = {};\n", "current task metadata");
  once(source, "      getRootTask() {\n", "root task");
  once(source, "      isErrored() { return this.#errored; }\n", "task error");
  const out =
    HEADER +
    PRELUDE +
    source
      .slice(HEADER.length)
      .replace(TASK_ANCHOR, TASK_ANCHOR + "      __tembraObserveTask(newTask, args);\n")
      .replace(TRAP_ANCHOR, TRAP_ANCHOR + "        __tembraObserveTrap(this.#trapError);\n");
  return out;
}

/**
 * JCO 0.23.0 vendor bug fix.
 *
 * In the canonical future/stream intrinsics (`futureRead`/`futureWrite`/`streamRead`/...), the
 * element lift metadata `_liftFlatOwn({ componentIdx, tableIdx, createResourceFn })` reads and
 * removes the guest handle from `handleTable<resource index>` instead of `handleTable<tableIdx>`.
 * The resource index equals the capture-table index, so the lift consults a table of another
 * component (`unknown handle index N`). Import-lowering sites already use the right table.
 * Effect: a guest that writes an owned imported resource into a future or stream read by the host
 * (for example `wasi:http` request trailers, `result<option<fields>, error-code>`) traps the read.
 *
 * Imported-resource sites must have the shape below, with a target table declared for the same
 * component and paired with the same capture table. Exported-resource sites instead retain
 * their handle until disposal; admit that distinct pinned shape unchanged only when its class,
 * table, finalization registry and destructor agree. Every other shape refuses the glue.
 */
const LIFT_OWN =
  /_liftFlatOwn\(\{\n\s*componentIdx: (\d+),\n\s*tableIdx: (\d+),\n\s*classNameFn: \(\) => [\w$]+,\n\s*createResourceFn: \n\s*\(handle\) => \{\n\s*const rep = handleTable(\d+)\[\(handle << 1\) \+ 1\] % T_OWN;\n\s*let resourceObj = captureTable(\d+)\.get\(rep\);\n(?:.*\n){1,8}?\s*rscTableRemove\(handleTable(\d+), handle\);\n/g;
const TABLE =
  /const handleTable(\d+) = \[T_FLAG, 0\];\n\s*handleTable\1\._createdReps = new Set\(\);\n\s*handleTable\1\._componentIdx = (\d+);\n\s*\n\s*\n\s*const captureTable(\d+)= new Map\(\);\n/g;
const EXPORTED_TABLE =
  /const handleTable(\d+) = \[T_FLAG, 0\];\n\s*handleTable\1\._createdReps = new Set\(\);\n\s*handleTable\1\._componentIdx = (\d+);\n\s*const finalizationRegistry(\d+) = finalizationRegistryCreate\(\(handle\) => \{\n\s*const \{ rep \} = rscTableRemove\(handleTable\1, handle\);\n\s*callResourceDestructor\(\{ componentIdx: \2, dtor: (exports\d+\["\d+"\]), rep \}\);\n\s*\}\);\n\s*HANDLE_TABLES\[\1\] = handleTable\1;\n/g;
const EXPORTED_LIFT_OWN = new RegExp(
  [
    String.raw`_liftFlatOwn\(\{\n\s*componentIdx: (\d+),\n\s*tableIdx: (\d+),\n\s*classNameFn: \(\) => ([\w$]+),\n\s*createResourceFn: \n\s*\(handle\) => \{\n`,
    String.raw`\s*const resourceObj = Object\.create\(\3\.prototype\);\n\s*Object\.defineProperty\(resourceObj, symbolRscHandle, \{\n\s*writable: true,\n\s*value: handle,\n\s*\}\);\n`,
    String.raw`\s*finalizationRegistry(\d+)\.register\(resourceObj, handle, resourceObj\);\n\s*Object\.defineProperty\(\n\s*resourceObj,\n\s*symbolDispose,\n\s*\{\n\s*writable: true,\n\s*value: function\(\) \{\n`,
    String.raw`\s*finalizationRegistry\4\.unregister\(resourceObj\);\n\s*const handleEntry = rscTableRemove\(handleTable\2, handle\);\n\s*resourceObj\[symbolDispose\] = emptyFunc;\n\s*resourceObj\[symbolRscHandle\] = undefined;\n`,
    String.raw`\s*callResourceDestructor\(\{ componentIdx: \1, dtor: (exports\d+\["\d+"\]), rep: handleEntry\.rep \}\);\n\s*\}\n\s*\}\n\s*\);\n\s*return resourceObj;\n\s*\}\n\s*,\n\s*\}\)`,
  ].join(""),
  "g",
);

export function fixLiftOwnTables(source: string): string {
  const tables = new Map<string, { component: string; capture: string }>();
  for (const [, table, component, capture] of source.matchAll(TABLE))
    tables.set(table!, { component: component!, capture: capture! });
  const exportedTables = new Map<string, { component: string; registry: string; dtor: string }>();
  for (const [, table, component, registry, dtor] of source.matchAll(EXPORTED_TABLE))
    exportedTables.set(table!, { component: component!, registry: registry!, dtor: dtor! });
  const classes = new Set([...source.matchAll(/\bclass ([\w$]+)\{/g)].map((match) => match[1]));
  const sites = source.split("_liftFlatOwn({").length - 1;
  let fixed = 0;
  for (const [, component, table, className, registry, dtor] of source.matchAll(
    EXPORTED_LIFT_OWN,
  )) {
    const declared = exportedTables.get(table!);
    if (
      !declared ||
      declared.component !== component ||
      declared.registry !== registry ||
      declared.dtor !== dtor ||
      !classes.has(className)
    )
      throw new Error(
        "unsupported trusted compiler output: exported owned-resource lift table (expected JCO 0.23.0)",
      );
    fixed++;
  }
  const out = source.replace(LIFT_OWN, (site, component, table, read, capture, removed) => {
    const declared = tables.get(table);
    if (
      !declared ||
      declared.component !== component ||
      declared.capture !== capture ||
      read !== removed
    )
      throw new Error(
        "unsupported trusted compiler output: owned-resource lift table (expected JCO 0.23.0)",
      );
    fixed++;
    return site
      .replace(`const rep = handleTable${read}[`, `const rep = handleTable${table}[`)
      .replace(
        `rscTableRemove(handleTable${removed}, handle)`,
        `rscTableRemove(handleTable${table}, handle)`,
      );
  });
  if (fixed !== sites)
    throw new Error(
      "unsupported trusted compiler output: owned-resource lift shape (expected JCO 0.23.0)",
    );
  return out;
}

/**
 * JCO 0.23.0 converts a typed result error inside the async wrapper's own try block. Its
 * catch then exits a task whose callback driver still owns guest work. Only that converted
 * error must bypass the early exit; genuine failures retain the vendor exit path.
 */
const THROW = "if (taskRes.tag === 'err') { throw new ComponentError(taskRes.val);}";
const MARKED =
  "if (taskRes.tag === 'err') { throw __tembraResultErr(new ComponentError(taskRes.val));}";
const EXIT = "if (!task.isExited()) { task.exit({ skipExclusiveLockCheck: true }); }";
const GUARDED =
  "if (!task.isExited() && !__tembraResultErrs.has(err)) { task.exit({ skipExclusiveLockCheck: true }); }";
const RESULT_ERR_PRELUDE = `  const __tembraResultErrs = new WeakSet();
  function __tembraResultErr(error) { __tembraResultErrs.add(error); return error; }
`;

export function fixResultErrExit(source: string): string {
  if (!source.startsWith(HEADER))
    throw new Error(
      "unsupported trusted compiler output: instantiate signature (expected JCO 0.23.0)",
    );
  const throws = source.split(THROW).length - 1;
  if (throws === 0) return source;
  const exits = source.split(EXIT).length - 1;
  if (exits === 0)
    throw new Error("unsupported trusted compiler output: result error exit (expected JCO 0.23.0)");
  const out =
    HEADER +
    RESULT_ERR_PRELUDE +
    source.slice(HEADER.length).split(THROW).join(MARKED).split(EXIT).join(GUARDED);
  if (out.includes(THROW) || out.includes(EXIT))
    throw new Error(
      "unsupported trusted compiler output: result error rewrite (expected JCO 0.23.0)",
    );
  return out;
}

/**
 * JCO's scheduler yields after each resumed thread before checking another thread. Nested
 * browser timers clamp that zero-delay turn to 4 ms. Use the browser's native task scheduler
 * when available, retaining a real event-loop turn so host timers, cancellation and microtask
 * continuations can run. Initial and idle polling remain on JCO's existing timers; no CABI
 * readiness, task ownership, rejection or exit handling changes.
 */
const SCHEDULER_TURN = `            // After resuming a task, re-tick as soon as the resumed
            // slice's microtask continuations have drained (timeout 0)
            // so queued sibling resumptions aren't charged the idle
            // polling interval; otherwise poll at the idle cadence.
            const delay = result === ComponentAsyncState.TickResult.RESUMED ? 0 : 10;
            await new Promise((resolve) => setTimeout(resolve, delay));
            result = this.tick();
`;
const SCHEDULER_TURN_PRELUDE = `  function __tembraSchedulerTurn(delay) {
    const timer = () => new Promise((resolve) => setTimeout(resolve, delay));
    if (delay === 0) {
      try {
        const scheduler = globalThis.scheduler;
        if (typeof scheduler?.postTask === 'function') {
          return scheduler.postTask(() => {}, { priority: 'user-visible' }).catch(timer);
        }
      } catch {
        // An unavailable host scheduling API retains the vendor timer path.
      }
    }
    return timer();
  }
`;

export function fixSchedulerTurns(source: string): string {
  if (!source.startsWith(HEADER))
    throw new Error(
      `unsupported trusted compiler output: instantiate signature (expected JCO ${JCO_VERSION})`,
    );
  once(source, "      async runTickLoop() {\n", "async scheduler");
  once(source, SCHEDULER_TURN, "resumed scheduler turn");
  return (
    HEADER +
    SCHEDULER_TURN_PRELUDE +
    source
      .slice(HEADER.length)
      .replace(
        SCHEDULER_TURN,
        SCHEDULER_TURN.replace(
          "await new Promise((resolve) => setTimeout(resolve, delay));",
          "await __tembraSchedulerTurn(delay);",
        ),
      )
  );
}

/** Apply every required rewrite to generated glue, in the order the anchors expect. */
export function prepareGlue(source: string): string {
  return fixSchedulerTurns(fixLiftOwnTables(fixResultErrExit(instrumentGlue(source))));
}
