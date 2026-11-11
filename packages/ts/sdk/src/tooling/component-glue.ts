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
 * Every `_liftFlatOwn({` site must have the exact shape below, its target table must be declared
 * for the same component, and that declaration must be paired with the same capture table;
 * otherwise the glue is refused (fail closed, like the other anchors).
 */
const LIFT_OWN =
  /_liftFlatOwn\(\{\n\s*componentIdx: (\d+),\n\s*tableIdx: (\d+),\n\s*classNameFn: \(\) => [\w$]+,\n\s*createResourceFn: \n\s*\(handle\) => \{\n\s*const rep = handleTable(\d+)\[\(handle << 1\) \+ 1\] % T_OWN;\n\s*let resourceObj = captureTable(\d+)\.get\(rep\);\n(?:.*\n){1,8}?\s*rscTableRemove\(handleTable(\d+), handle\);\n/g;
const TABLE =
  /const handleTable(\d+) = \[T_FLAG, 0\];\n\s*handleTable\1\._createdReps = new Set\(\);\n\s*handleTable\1\._componentIdx = (\d+);\n\s*\n\s*\n\s*const captureTable(\d+)= new Map\(\);\n/g;

export function fixLiftOwnTables(source: string): string {
  const tables = new Map<string, { component: string; capture: string }>();
  for (const [, table, component, capture] of source.matchAll(TABLE))
    tables.set(table!, { component: component!, capture: capture! });
  const sites = source.split("_liftFlatOwn({").length - 1;
  let fixed = 0;
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

/** Apply every required rewrite to generated glue, in the order the anchors expect. */
export function prepareGlue(source: string): string {
  return fixLiftOwnTables(fixResultErrExit(instrumentGlue(source)));
}
