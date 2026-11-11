import { describe, expect, test } from "bun:test";
import { fixLiftOwnTables, fixSchedulerTurns } from "../src/tooling/component-glue.ts";

const HEADER =
  '"use components";\nexport function instantiate(getCoreModule, imports, instantiateCore, options) {\n';
// The pinned vendor scheduler seam. Lifecycle behavior is qualified with real prepared components.
const SCHEDULER = `      async runTickLoop() {
            // After resuming a task, re-tick as soon as the resumed
            // slice's microtask continuations have drained (timeout 0)
            // so queued sibling resumptions aren't charged the idle
            // polling interval; otherwise poll at the idle cadence.
            const delay = result === ComponentAsyncState.TickResult.RESUMED ? 0 : 10;
            await new Promise((resolve) => setTimeout(resolve, delay));
            result = this.tick();
`;
const source = HEADER + SCHEDULER;

/** Execute the emitted helper itself, with independent host task queues under test control. */
function emitted(host: object = {}) {
  const queued: { callback: () => void; delay: number }[] = [];
  const output = fixSchedulerTurns(source);
  const helper = output.slice(HEADER.length, output.indexOf("      async runTickLoop() {"));
  const turn = new Function("globalThis", "setTimeout", helper + "return __tembraSchedulerTurn;")(
    host,
    (callback: () => void, delay: number) => queued.push({ callback, delay }),
  ) as (delay: number) => Promise<void>;
  return { turn, queued };
}

describe("pinned JCO scheduler turn rewrite", () => {
  test("changes only the resumed turn await and adds the per-instance helper", () => {
    const output = fixSchedulerTurns(source);
    expect(output).toContain("await __tembraSchedulerTurn(delay);");
    expect(output).toContain("result === ComponentAsyncState.TickResult.RESUMED ? 0 : 10");
    expect(output.slice(output.indexOf("      async runTickLoop() {"))).toBe(
      SCHEDULER.replace(
        "await new Promise((resolve) => setTimeout(resolve, delay));",
        "await __tembraSchedulerTurn(delay);",
      ),
    );
  });

  test("refuses missing, duplicated, changed or already rewritten anchors", () => {
    for (const changed of [
      source.replace(HEADER, ""),
      source.replace("      async runTickLoop() {\n", ""),
      source + SCHEDULER,
      source.replace("? 0 : 10", "? 0 : 20"),
      source.replace("setTimeout(resolve, delay)", "queueMicrotask(resolve)"),
      fixSchedulerTurns(source),
    ])
      expect(() => fixSchedulerTurns(changed)).toThrow("unsupported trusted compiler output");
  });

  test("unsupported hosts retain the vendor timer", async () => {
    for (const host of [
      {},
      { scheduler: null },
      { scheduler: {} },
      { scheduler: { postTask: true } },
      {
        get scheduler() {
          throw new Error("unavailable scheduler");
        },
      },
      {
        scheduler: {
          get postTask() {
            throw new Error("unavailable postTask");
          },
        },
      },
    ]) {
      const { turn, queued } = emitted(host);
      const pending = turn(0);
      expect(queued).toHaveLength(1);
      expect(queued[0]!.delay).toBe(0);
      queued[0]!.callback();
      await pending;
    }
  });

  test("idle polling bypasses the native scheduler and retains its delay", async () => {
    const { turn, queued } = emitted({
      scheduler: {
        postTask() {
          throw new Error("unused");
        },
      },
    });
    const pending = turn(10);
    expect(queued).toHaveLength(1);
    expect(queued[0]!.delay).toBe(10);
    queued[0]!.callback();
    await pending;
  });

  test("a native task preserves its receiver, priority and asynchronous boundary", async () => {
    const tasks: (() => void)[] = [];
    const scheduler = {
      postTask(callback: () => void, options: { priority: string }) {
        expect(this).toBe(scheduler);
        expect(options).toEqual({ priority: "user-visible" });
        return new Promise<void>((resolve) =>
          tasks.push(() => {
            callback();
            resolve();
          }),
        );
      },
    };
    const { turn, queued } = emitted({ scheduler });
    let continued = false;
    const pending = turn(0).then(() => {
      continued = true;
    });
    expect(tasks).toHaveLength(1);
    expect(queued).toHaveLength(0);
    // Draining microtasks must not resume a component before the browser task runs.
    await Promise.resolve();
    expect(continued).toBe(false);
    tasks[0]!();
    await pending;
    expect(continued).toBe(true);
  });

  test("concurrent instances keep separate scheduled turns", async () => {
    const tasks: (() => void)[] = [];
    const host = {
      scheduler: { postTask: () => new Promise<void>((resolve) => tasks.push(resolve)) },
    };
    const first = emitted(host);
    const second = emitted(host);
    const completed: string[] = [];
    const a = first.turn(0).then(() => completed.push("first"));
    const b = second.turn(0).then(() => completed.push("second"));
    expect(tasks).toHaveLength(2);
    tasks[1]!();
    await b;
    expect(completed).toEqual(["second"]);
    tasks[0]!();
    await a;
    expect(completed).toEqual(["second", "first"]);
    expect(first.queued).toHaveLength(0);
    expect(second.queued).toHaveLength(0);
  });

  test("native scheduling throws and rejections fall back to a real timer turn", async () => {
    for (const postTask of [
      () => {
        throw new Error("unavailable");
      },
      () => Promise.reject(new Error("unavailable")),
    ]) {
      const { turn, queued } = emitted({ scheduler: { postTask } });
      let continued = false;
      const pending = turn(0).then(() => {
        continued = true;
      });
      await Promise.resolve();
      expect(queued).toHaveLength(1);
      expect(queued[0]!.delay).toBe(0);
      expect(continued).toBe(false);
      queued[0]!.callback();
      await pending;
      expect(continued).toBe(true);
    }
  });
});

const EXPORTED_TABLE = `class Session{}
const handleTable0 = [T_FLAG, 0];
handleTable0._createdReps = new Set();
handleTable0._componentIdx = 0;
const finalizationRegistry0 = finalizationRegistryCreate((handle) => {
  const { rep } = rscTableRemove(handleTable0, handle);
  callResourceDestructor({ componentIdx: 0, dtor: exports0["4"], rep });
});

HANDLE_TABLES[0] = handleTable0;
`;
// JCO's exported Session lift for the resource provider's async open-later factory.
const EXPORTED_LIFT = `_liftFlatOwn({
  componentIdx: 0,
  tableIdx: 0,
  classNameFn: () => Session,
  createResourceFn:${" "}
  (handle) => {
    const resourceObj = Object.create(Session.prototype);
    Object.defineProperty(resourceObj, symbolRscHandle, {
      writable: true,
      value: handle,
    });
    finalizationRegistry0.register(resourceObj, handle, resourceObj);

    Object.defineProperty(
    resourceObj,
    symbolDispose,
    {
      writable: true,
      value: function() {
        finalizationRegistry0.unregister(resourceObj);
        const handleEntry = rscTableRemove(handleTable0, handle);
        resourceObj[symbolDispose] = emptyFunc;
        resourceObj[symbolRscHandle] = undefined;
        callResourceDestructor({ componentIdx: 0, dtor: exports0["4"], rep: handleEntry.rep });
      }
    }
    );

    return resourceObj;
  }
  ,
})`;
// An imported resource whose capture index differs from its canonical handle table.
const IMPORTED_LIFT = `const handleTable7 = [T_FLAG, 0];
handleTable7._createdReps = new Set();
handleTable7._componentIdx = 2;


const captureTable4= new Map();
_liftFlatOwn({
  componentIdx: 2,
  tableIdx: 7,
  classNameFn: () => Fields,
  createResourceFn:${" "}
  (handle) => {
    const rep = handleTable4[(handle << 1) + 1] % T_OWN;
    let resourceObj = captureTable4.get(rep);
    if (!resourceObj) {
      resourceObj = Object.create(Fields.prototype);
      Object.defineProperty(resourceObj, symbolRscHandle, { writable: true, value: handle });
      Object.defineProperty(resourceObj, symbolRscRep, { writable: true, value: rep });
    } else {
      captureTable4.delete(rep);
    }
    rscTableRemove(handleTable4, handle);
    return resourceObj;
  }
  ,
})`;

describe("pinned JCO owned-resource lifts", () => {
  test("admits an exported constructor lift without rewriting its ownership or disposal", () => {
    const source = EXPORTED_TABLE + EXPORTED_LIFT;
    expect(fixLiftOwnTables(source)).toBe(source);
    // Registry and table numbering are independent; their declaration establishes the pairing.
    const renumbered = source
      .replaceAll("handleTable0", "handleTable8")
      .replaceAll("finalizationRegistry0", "finalizationRegistry3")
      .replace("HANDLE_TABLES[0]", "HANDLE_TABLES[8]")
      .replace("tableIdx: 0", "tableIdx: 8");
    expect(fixLiftOwnTables(renumbered)).toBe(renumbered);
  });

  test("repairs imported capture-table lifts alongside unchanged exported-resource lifts", () => {
    const imported = fixLiftOwnTables(IMPORTED_LIFT);
    expect(imported).toContain("const rep = handleTable7[(handle << 1) + 1] % T_OWN;");
    expect(imported).toContain("rscTableRemove(handleTable7, handle);");
    expect(imported).toContain("captureTable4.get(rep)");
    expect(fixLiftOwnTables(EXPORTED_TABLE + EXPORTED_LIFT + IMPORTED_LIFT)).toBe(
      EXPORTED_TABLE + EXPORTED_LIFT + imported,
    );
  });

  test("exported metadata, prototype, disposal and destructor drift are refused", () => {
    for (const site of [
      EXPORTED_LIFT.replace("componentIdx: 0", "componentIdx: 1"),
      EXPORTED_LIFT.replace("tableIdx: 0", "tableIdx: 1"),
      EXPORTED_LIFT.replace("() => Session", "() => Other"),
      EXPORTED_LIFT.replace("Session.prototype", "Other.prototype"),
      EXPORTED_LIFT.replace("finalizationRegistry0.unregister", "finalizationRegistry1.unregister"),
      EXPORTED_LIFT.replace("handleTable0, handle", "handleTable1, handle"),
      EXPORTED_LIFT.replace('exports0["4"]', 'exports0["5"]'),
      EXPORTED_LIFT.replace("resourceObj[symbolDispose] = emptyFunc;", ""),
      EXPORTED_LIFT.replace("return resourceObj;", "return other;"),
    ])
      expect(() => fixLiftOwnTables(EXPORTED_TABLE + site)).toThrow(
        "unsupported trusted compiler output",
      );
  });

  test("an export lift requires the matching declared class, table, registry and destructor", () => {
    for (const declaration of [
      "",
      EXPORTED_TABLE.replace("class Session{}", "class Other{}"),
      EXPORTED_TABLE.replace("_componentIdx = 0", "_componentIdx = 1"),
      EXPORTED_TABLE.replace("finalizationRegistry0", "finalizationRegistry1"),
      EXPORTED_TABLE.replace('exports0["4"]', 'exports0["5"]'),
      EXPORTED_TABLE.replace("HANDLE_TABLES[0]", "HANDLE_TABLES[1]"),
    ])
      expect(() => fixLiftOwnTables(declaration + EXPORTED_LIFT)).toThrow(
        "exported owned-resource lift table",
      );
  });

  test("capture-table mismatches and unknown owned-resource shapes remain refused", () => {
    for (const source of [
      IMPORTED_LIFT.replace("_componentIdx = 2", "_componentIdx = 1"),
      IMPORTED_LIFT.replace("captureTable4=", "captureTable5="),
      IMPORTED_LIFT.replace("rscTableRemove(handleTable4", "rscTableRemove(handleTable5"),
      EXPORTED_TABLE + EXPORTED_LIFT + "_liftFlatOwn({ unknown: true })",
    ])
      expect(() => fixLiftOwnTables(source)).toThrow("unsupported trusted compiler output");
  });
});
