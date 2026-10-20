import {
  Type,
  defineFacet,
  defineHook,
  defineRegistry,
  local,
  method,
  native,
  opaque,
} from "@tembra/sdk";
import type { TembraHost } from "@tembra/sdk";

import { check, deferredVoid } from "./assertions.ts";

/** Exercises an abandoned hook promise using the actual platform binding and Rust Host. */
export async function detachedHookOwnership(create: () => Promise<TembraHost>): Promise<void> {
  for (const action of ["release", "reload", "close"] as const) {
    // oxlint-disable-next-line no-await-in-loop -- Each independent Host must finish physical cleanup before the next case.
    await detachedHookCase(create, action);
  }
}
async function detachedHookCase(
  create: () => Promise<TembraHost>,
  action: "release" | "reload" | "close",
): Promise<void> {
  const host = await create();
  const entered = deferredVoid();
  const gate = deferredVoid();
  const observed = deferredVoid();
  const capability = native<{ marker: symbol }>("test.detached-hook.capability@1");
  const hook = defineHook("test.detached-hook", {
    version: 1,
    strategy: "series",
    input: Type.Integer(),
    terminal: { input: Type.Integer(), result: Type.Integer() },
  });
  const Caller = local("test.detached-hook.caller", {
    version: 1,
    input: Type.Object({}),
    members: {
      count: Type.Integer(),
      start: method(),
      value: opaque<{ marker: symbol }>(capability.native),
    },
  });
  const Terminal = local("test.detached-hook.terminal", {
    version: 1,
    input: Type.Object({}),
    members: { run: method(Type.Integer(), Type.Integer()) },
  });
  const terminals = defineRegistry("test.detached-hook.terminals");
  let originClosed = false;
  let callbackFinished = false;
  let detachedStarted = false;
  let currentValue: { marker: symbol } | undefined;
  const callerFacet = (revision: number) =>
    defineFacet({
      name: "detached-hook-caller",
      uses: [],
      setup(scope, declaration) {
        declaration.serve(Caller, (_input, self) => {
          const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
          const resource = self.resource("origin", {
            version: revision,
            open() {
              currentValue = { marker: Symbol("origin") };
              return currentValue;
            },
            close() {
              originClosed = true;
            },
          });
          return {
            state: { count },
            opaque: { value: resource },
            methods: {
              async start(context) {
                count.replace(context, count.value + 1);
                const terminal = scope.registry(terminals).table(context).get("run")?.operation;
                check(terminal, "managed detached-hook terminal");
                // Deliberately abandon the returned hook promise while its native job remains active.
                const pending = scope.runHook(hook, context, 7, { terminal });
                detachedStarted = true;
                void pending.then(observed.resolve, observed.resolve);
                await entered.promise;
                return undefined;
              },
            },
          };
        });
      },
    });
  host.registerHook(hook);
  host.registerOpaque(capability);
  let finishTransition: Promise<unknown> | undefined;
  let reference: Awaited<ReturnType<typeof host.get<typeof Caller>>> | undefined;
  try {
    await host.install(
      defineFacet({
        name: "detached-hook-terminal",
        uses: [],
        setup(_scope, declaration) {
          declaration.serve(
            Terminal,
            (_input, self) => {
              self.registerMethod(terminals, "run", {}, "run");
              return {
                state: {},
                methods: {
                  async run(_context, value) {
                    entered.resolve();
                    await gate.promise;
                    callbackFinished = true;
                    return value * 2;
                  },
                },
              };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    await host.install(callerFacet(1));
    reference = await host.get(Caller, {});
    const original = reference.value;
    check(original === currentValue, "origin opaque identity before detached work");
    await reference.start();
    check(
      reference.count.value === 1,
      "origin state remains readable after logical method completion",
    );
    let transitioned = false;
    if (action === "release") {
      reference.release();
    } else {
      finishTransition = (action === "reload" ? host.reload([callerFacet(2)]) : host.close()).then(
        () => {
          transitioned = true;
        },
      );
    }
    // Real scheduling turns let disposal/reload attempt retirement; the terminal intentionally ignores cancellation.
    // oxlint-disable-next-line promise/avoid-new -- The test observes the physical gate after the real runtime has had a scheduling turn.
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 30);
    });
    check(
      !originClosed,
      `${action}: originating resource must remain open until detached hook physically settles`,
    );
    check(!callbackFinished, `${action}: physical terminal remains gated`);
    if (action === "close") {
      check(!transitioned, `${action}: lifecycle joins originating physical invocation`);
    }
    gate.resolve();
    await observed.promise;
    await finishTransition;
    check(callbackFinished, `${action}: terminal physically completed before lifecycle settlement`);
    if (action === "reload") {
      check(reference.count.value === 1, "reload preserves retained reference and state");
      check(
        reference.value !== original,
        "new resource generation becomes visible after retirement",
      );
    }
  } finally {
    gate.resolve();
    if (detachedStarted) {
      await observed.promise;
    }
    await finishTransition;
    if (action !== "close") {
      reference?.release();
    }
    await host.close();
  }
  check(originClosed, `${action}: origin resource cleanup eventually completes`);
  const diagnostics = host.diagnostics();
  check(diagnostics.jobs === 0, `${action}: no managed jobs remain after close`);
  if ("callbacks" in diagnostics) {
    check(diagnostics.callbacks === 0, `${action}: no browser callback owners remain`);
  }
  if ("bridge" in diagnostics) {
    const { bridge } = diagnostics;
    check(bridge !== null && typeof bridge === "object", "binding bridge diagnostics");
    check("jobs" in bridge && bridge.jobs === 0, `${action}: no Node bridge jobs remain`);
    check(
      "callbacks" in bridge && bridge.callbacks === 0,
      `${action}: no Node callback owners remain`,
    );
    check("values" in bridge && bridge.values === 0, `${action}: no Node opaque values remain`);
  }
}
