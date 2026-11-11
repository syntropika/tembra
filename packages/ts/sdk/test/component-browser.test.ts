import { describe, expect, test } from "bun:test";
import { converters, type TypeGraph } from "../src/interpreter/component/values.ts";
import { ComponentPort } from "../src/interpreter/component/port.ts";
import {
  checkPublicWasi,
  runtimeInternal,
  runtimeInternals,
} from "../src/interpreter/component/wasi.ts";
import type { ExecutionScope } from "../src/interpreter/types.ts";

const graph: TypeGraph = {
  javascriptNames: { "big-value": "bigValue", items: "items" },
  types: [
    { id: 0, definition: { kind: { option: "u64" } } },
    { id: 1, definition: { kind: { list: "u8" } } },
    { id: 2, definition: { kind: { result: { ok: "u64", err: "string" } } } },
    {
      id: 3,
      definition: {
        kind: {
          record: {
            fields: [
              { name: "big-value", type: 0 },
              { name: "items", type: 1 },
            ],
          },
        },
      },
    },
    { id: 4, definition: { kind: { option: 0 } } },
    {
      id: 5,
      definition: {
        kind: {
          variant: {
            cases: [{ name: "none-case" }, { name: "some-case", type: "u64" }],
          },
        },
      },
    },
    { id: 6, definition: { kind: { stream: "u8" } } },
  ],
};
const convert = converters(graph);

describe("component value conversion", () => {
  test("options, typed lists and records round trip between domain and JCO forms", () => {
    const fn = convert({ params: [{ name: "value", type: 3 }], result: 3 });
    const domain = { bigValue: null, items: [1, 2, 255] };
    const [guest] = fn.guestArguments([domain]) as [{ bigValue: unknown; items: Uint8Array }];
    expect(guest.bigValue).toBeUndefined();
    expect(guest.items).toBeInstanceOf(Uint8Array);
    expect(fn.hostArguments([guest])).toEqual([domain]);
    expect(
      fn.hostArguments([{ bigValue: 18446744073709551615n, items: Uint8Array.of(7) }]),
    ).toEqual([{ bigValue: 18446744073709551615n, items: [7] }]);
  });

  test("nested options use JCO's tagged outer level", () => {
    const fn = convert({ params: [{ name: "value", type: 4 }] });
    expect(fn.guestArguments([null])).toEqual([{ tag: "none" }]);
    expect(fn.guestArguments([5n])).toEqual([{ tag: "some", val: 5n }]);
    expect(fn.hostArguments([{ tag: "some", val: undefined }])).toEqual([null]);
  });

  test("a result return is re-tagged; only JCO's typed err lane is recoverable", async () => {
    const fn = convert({ params: [], result: 2 });
    expect(await fn.fromGuest(() => 3n)).toEqual({ tag: "ok", val: 3n });
    const payload = Object.assign(new Error("typed"), { payload: "denied" });
    expect(
      await fn.fromGuest(() => {
        throw payload;
      }),
    ).toEqual({ tag: "err", val: "denied" });
    await expect(
      fn.fromGuest(() => {
        throw new Error("trap");
      }),
    ).rejects.toThrow("trap");
    expect(fn.toGuest({ tag: "err", val: "no" })).toEqual({
      tag: "err",
      val: "no",
    });
  });

  test("payload-less variant cases carry no val in the host form", () => {
    const fn = convert({ params: [{ name: "value", type: 5 }] });
    expect(fn.hostArguments([{ tag: "none-case", val: undefined }])).toEqual([
      { tag: "none-case" },
    ]);
    expect(() => fn.hostArguments([{ tag: "other" }])).toThrow("Unknown WIT case");
  });

  test("unsupported component values are refused at preparation, not silently dropped", () => {
    expect(() => convert({ params: [{ name: "value", type: 6 }] })).toThrow(
      "unavailable in browser component execution",
    );
  });
});

/** An execution scope that records cancellation. */
function scope(): ExecutionScope & { cancelled: boolean } {
  return {
    cancelled: false,
    check() {},
    cancel() {
      this.cancelled = true;
    },
    sync: () => null,
    async: async () => null,
  };
}

/**
 * A port whose callbacks behave like the instrumented glue: the export task starts synchronously
 * at entry and exits after its result (`exitDelay` later), or exits rejected when it throws.
 */
let nextTask = 1;
class ObservedPort extends ComponentPort {
  exitDelay = 0;
  override run<T>(environment: bigint, scope: ExecutionScope, call: () => T | Promise<T>) {
    return super.run(environment, scope, () => {
      const id = nextTask++;
      this.observer.taskStarted({ id, entry: "test", isAsync: true });
      let result: Promise<T>;
      try {
        result = Promise.resolve(call());
      } catch (error) {
        result = Promise.reject(error);
      }
      result.then(
        () => setTimeout(() => this.observer.taskExited({ id }), this.exitDelay),
        (error) => this.observer.taskExited({ id, rejected: true, error }),
      );
      return result;
    });
  }
}

describe("component port", () => {
  const port = () => {
    const created = new ObservedPort(200, async () => {});
    created.exports = {};
    return created;
  };

  test("a selection is consumed by the next import of its kind, and none clears it", async () => {
    const instance = port();
    await instance.run(1n, scope(), () => {
      instance.select("reference", { id: 1 });
      expect(instance.take(instance.current(), "reference")).toEqual({ id: 1 });
      instance.select("reference", { id: 2 });
      instance.select("reference", undefined);
      expect(instance.take(instance.current(), "reference")).toBeUndefined();
    });
    expect(instance.usable).toBe(true);
  });

  test("a selection left past its turn refuses the callback without quarantine", async () => {
    const instance = port();
    await expect(
      instance.run(1n, scope(), async () => {
        instance.select("reference", { id: 1 });
        await Promise.resolve();
      }),
    ).rejects.toThrow("not consumed");
    expect(instance.usable).toBe(true);
  });

  test("detached host work stays attributed to its callback until it settles", async () => {
    const instance = port();
    const first = scope();
    let observed: bigint | undefined;
    await instance.run(7n, first, () => {
      const callback = instance.current();
      instance.physical(
        callback,
        new Promise((resolve) => setTimeout(resolve, 20)).then(() => {
          observed = instance.current().environment;
        }),
      );
      return "returned";
    });
    expect(observed).toBe(7n);
    expect(first.cancelled).toBe(true);
    expect(() => instance.current()).toThrow("No active host scope");
  });

  test("a callback keeps admission until its task really exits", async () => {
    const instance = port() as ObservedPort;
    instance.exitDelay = 30;
    const started = performance.now();
    expect(await instance.run(1n, scope(), () => "early result")).toBe("early result");
    expect(performance.now() - started).toBeGreaterThanOrEqual(25);
    expect(await instance.run(2n, scope(), () => 2)).toBe(2);
  });

  test("a result is not physical completion: never-ending detached work hits the deadline", async () => {
    const instance = port();
    const started = performance.now();
    await expect(
      instance.run(1n, scope(), () => {
        // The task exits after its result, but its host operation never settles.
        instance.physical(instance.current(), new Promise(() => {}));
        return 0;
      }),
    ).rejects.toThrow("wall time");
    expect(performance.now() - started).toBeGreaterThanOrEqual(190);
    expect(instance.usable).toBe(false);
  });

  test("a guest destructor keeps its callback admitted until the provider closes", async () => {
    const instance = port();
    const entered = Promise.withResolvers<void>();
    const close = Promise.withResolvers<void>();
    const events: string[] = [];
    let completed = false;
    const first = instance
      .run(1n, scope(), () => {
        instance.wasi.cleanup(async () => {
          entered.resolve();
          await close.promise;
          events.push("closed");
        });
        return 1;
      })
      .then((value) => {
        completed = true;
        return value;
      });
    await entered.promise;
    // Let the already queued task-exit notification run; provider completion is separately gated.
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(completed).toBe(false);
    await expect(instance.run(2n, scope(), () => 2)).rejects.toThrow("busy");
    close.resolve();
    expect(await first).toBe(1);
    expect(
      await instance.run(2n, scope(), () => {
        events.push("next");
        return 2;
      }),
    ).toBe(2);
    expect(events).toEqual(["closed", "next"]);
    await instance.close();
  });

  test("a failed guest destructor fails the callback and quarantines the revision", async () => {
    const instance = port();
    await expect(
      instance.run(1n, scope(), () => {
        instance.wasi.cleanup(async () => {
          throw new Error("provider close failed");
        });
        return 1;
      }),
    ).rejects.toThrow("provider close failed");
    expect(instance.usable).toBe(false);
    await expect(instance.run(2n, scope(), () => 2)).rejects.toThrow("quarantined");
    await instance.close();
  });

  test("a cancelled wait cannot restart under a later callback", async () => {
    const instance = port();
    const first = scope();
    let issued: bigint | undefined;
    await instance.run(1n, first, () => 1);
    // The first callback's authority is revoked when it ends: its scope is cancelled.
    expect(first.cancelled).toBe(true);
    await instance.run(2n, scope(), () => {
      issued = instance.current().environment;
      return 2;
    });
    expect(issued).toBe(2n);
  });

  test("a task that exits rejected quarantines the instance", async () => {
    const instance = port();
    await expect(
      instance.run(1n, scope(), () => {
        throw new Error("guest trap");
      }),
    ).rejects.toThrow("guest trap");
    expect(instance.usable).toBe(false);
  });

  test("a trap or timeout quarantines the instance and refuses re-entry", async () => {
    const instance = port();
    await expect(instance.run(1n, scope(), () => new Promise(() => {}))).rejects.toThrow(
      "wall time",
    );
    expect(instance.usable).toBe(false);
    await expect(instance.run(1n, scope(), () => 1)).rejects.toThrow("quarantined");
  });
});

describe("component WASI", () => {
  test("runtime internals are exact WASI 0.2 names and never see grants", () => {
    expect(runtimeInternal("wasi:clocks/monotonic-clock@0.2.12")).toBe(true);
    expect(runtimeInternal("wasi:clocks/monotonic-clock@0.3.0")).toBe(false);
    expect(runtimeInternal("wasi:sockets/tcp@0.2.12")).toBe(false);
    const imports = runtimeInternals(
      [
        "wasi:clocks/monotonic-clock@0.2.12",
        "wasi:cli/environment@0.2.12",
        "wasi:clocks/monotonic-clock@0.3.0",
      ],
      undefined,
    ) as Record<string, Record<string, () => unknown>>;
    expect(Object.keys(imports)).toEqual([
      "wasi:clocks/monotonic-clock@0.2.12",
      "wasi:cli/environment@0.2.12",
    ]);
    expect(imports["wasi:clocks/monotonic-clock@0.2.12"]!.now!()).toBe(0n);
    expect(imports["wasi:cli/environment@0.2.12"]!.getEnvironment!()).toEqual([]);
  });

  test("public WASI the browser cannot serve yet is refused before instantiation", () => {
    expect(() => checkPublicWasi(["wasi:clocks/monotonic-clock@0.3.0"])).not.toThrow();
    expect(() => checkPublicWasi(["wasi:unknown/thing@0.3.0"])).toThrow(
      "does not provide wasi:unknown/thing@0.3.0",
    );
  });
});
