import { Type, defineFacet, defineRegistry, local, method } from "@tembra/sdk";
import type { EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/browser";
import { assertQuiescent } from "@tembra/testing";
import {
  portableHooks,
  javascriptOpaque,
  hookBoundaries,
  hookPhysicalSettlement,
} from "@tembra/testing/conformance";

import { detachedHookOwnership } from "../../src/detached-hooks.js";
import { browserManagerAcceptance, browserManagerLiveness } from "./browser-managers.js";
import { browserModuleAcceptance } from "./browser-modules.js";
import { browserOptionsAcceptance } from "./browser-options.js";

function check(condition: boolean, message: string): asserts condition {
  if (!condition) {
    throw new Error(message);
  }
}
const Counter = local("test.browser.counter", {
  version: 1,
  input: Type.Object({}),
  members: { count: Type.Integer(), increment: method(undefined, Type.Integer()) },
});
const Commands = defineRegistry<{ title: string; description?: string }>("test.browser.commands", {
  conflict: "error",
});
async function compiled(step: number): Promise<EsmArtifactV1> {
  const source = `const Counter={descriptor:${JSON.stringify(Counter.descriptor)}};
  export default {name:'dynamic-counter',uses:[],setup(_s,d){d.serve(Counter,(_i,self)=>{
    const count=self.cell('count',{schema:{type:'integer'},initial:0});
    return {state:{count},methods:{increment(ctx){return count.replace(ctx,count.value+${step}).value}}};
  });}};`;
  const hash = new Uint8Array(
    await crypto.subtle.digest("SHA-256", new TextEncoder().encode(source)),
  );
  return {
    format: "tembra-esm-v1",
    identity: "browser-counter",
    source,
    sha256: Array.from(hash, (byte) => byte.toString(16).padStart(2, "0")).join(""),
    exports: ["default"],
  };
}

export async function browserAcceptance(): Promise<string[]> {
  const passed: string[] = [];
  const host = await createHost({ name: "acceptance" });
  let opened: object | undefined;
  let closed: unknown;
  try {
    await host.install(
      defineFacet({
        name: "counter",
        uses: [],
        setup(_scope, d) {
          d.serve(
            Counter,
            (_i, self) => {
              const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
              self.resource("resource", {
                open: () => {
                  opened = {};
                  return opened;
                },
                close: (value) => {
                  closed = value;
                  return 1n;
                },
              });
              self.registerMethod(Commands, "increment", { title: "Increment" }, "increment");
              return {
                state: { count },
                methods: {
                  increment(ctx) {
                    return count.replace(ctx, count.value + 1).value;
                  },
                },
              };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    const reference = await host.get(Counter, {});
    let observed = 0;
    const stop = reference.count.subscribe((value) => {
      observed = value;
    });
    check((await reference.increment()) === 1 && observed === 1, "state subscription");
    await host.root(async (ctx) => {
      const snapshot = host.registry(Commands).table(ctx);
      const entry = snapshot.get("increment");
      const title: string | undefined = entry?.data.title;
      check(title === "Increment" && Object.isFrozen(entry?.data), "typed registry metadata");
      check(Object.isFrozen(snapshot.entries), "immutable registry snapshot");
      const operation = entry?.operation;
      check(operation !== undefined, "registry operation");
      const result = await operation.call(ctx, { present: false });
      check(result.present && result.value === 2, "registry dispatch");
    });
    // Resource dependencies are object-owned and must be declared through self.get.
    const Consumer = local("test.browser.consumer", {
      version: 1,
      input: Type.Object({}),
      members: {},
    });
    await host.install(
      defineFacet({
        name: "consumer",
        uses: [Counter],
        setup(_scope, d) {
          d.serve(
            Consumer,
            (_i, self) => {
              self.resource("dependency", {
                deps: [self.get(Counter, {})],
                open: () => ({}),
                close: () => undefined,
              });
              self.derive([reference.count], Type.Integer(), ([value]) => value.value * 2);
              return { state: {}, methods: {} };
            },
            { lifetime: "host" },
          );
        },
      }),
    );
    stop();
    reference.release();
    check(reference.status === "released", "released status");
    await host.close();
    await host.close();
    const diagnostics = host.diagnostics();
    check(
      typeof diagnostics.callbacks === "number" && typeof diagnostics.jobs === "number",
      "managed diagnostics",
    );
    assertQuiescent({ callbacks: diagnostics.callbacks, jobs: diagnostics.jobs });
    check(opened !== undefined && opened === closed, "resource identity and close");
    passed.push(
      "callbacks, subscriptions, managed roots, registries, dependencies, resources and idempotent close",
    );
  } finally {
    await host.close();
  }
  const failures = await createHost();
  try {
    await failures.install(
      defineFacet({
        name: "retained-errors",
        uses: [],
        setup(_scope, declaration) {
          declaration.serve(Counter, (_input, self) => {
            const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
            self.task(() => {
              throw new Error("background diagnostic failure");
            });
            return {
              state: { count },
              methods: {
                increment(ctx) {
                  return count.replace(ctx, count.value + 1).value;
                },
              },
            };
          });
        },
      }),
    );
    const reference = await failures.get(Counter, {});
    const stop = reference.onShape(() => {
      throw Object.assign(new Error("callback diagnostic failure"), {
        code: "invalid",
        reason: "diagnostic_callback",
        detailsJson: '{"nested":{"retained":true}}',
      });
    });
    for (let attempt = 0; attempt < 50 && failures.diagnostics().errors.length < 2; attempt += 1) {
      // oxlint-disable-next-line no-await-in-loop, promise/avoid-new -- Let the real WASM task settle before observing the retained core log.
      await new Promise<void>((resolve) => {
        setTimeout(resolve, 2);
      });
    }
    const live = failures.diagnostics();
    check(
      live.errors.some((error) => error.message === "background diagnostic failure"),
      "retained background error while live",
    );
    const callback = live.errors.find((error) => error.reason === "diagnostic_callback");
    check(
      callback?.message === "callback diagnostic failure" &&
        JSON.stringify(callback.details) === '{"nested":{"retained":true}}',
      "structured callback error while live",
    );
    check(
      Object.isFrozen(live.errors) && Object.isFrozen(callback.details),
      "immutable error diagnostics",
    );
    stop();
    reference.release();
    await failures.close();
    const finalSnapshot = failures.diagnostics();
    check(
      JSON.stringify(finalSnapshot.errors) === JSON.stringify(live.errors),
      "retained errors after Host close",
    );
    check(failures.diagnostics() === finalSnapshot, "stable final diagnostics snapshot");
    passed[0] += ", retained callback/background failures in live and closed diagnostics";
  } finally {
    await failures.close();
  }
  const dynamic = await createHost();
  const second = await createHost();
  try {
    await dynamic.load(await compiled(1));
    const reference = await dynamic.get(Counter, {});
    check((await reference.increment()) === 1, "compiled increment");
    const report = await dynamic.reload([await compiled(2)]);
    check(
      report.codeVersion === 2 && (await reference.increment()) === 3,
      "retained state after reload",
    );
    let missing = false;
    try {
      await second.get(Counter, {});
    } catch {
      missing = true;
    }
    check(missing, "independent Host provider state");
    reference.release();
    passed.push("compiled ESM, retained references and state across reload, independent Hosts");
  } finally {
    await dynamic.close();
    await second.close();
  }
  const cancellation = await createHost();
  let entered: (() => void) | undefined;
  // oxlint-disable-next-line promise/avoid-new -- The test controls entry into a root before asking shutdown to cancel it.
  const started = new Promise<void>((resolve) => {
    entered = resolve;
  });
  const root = cancellation.root(async (ctx) => {
    entered?.();
    await ctx.signal.cancelled;
  });
  await started;
  await cancellation.close();
  await root;
  passed.push("shutdown cancels and joins a cooperative root");
  const hooks = await createHost();
  try {
    await portableHooks(hooks);
    await javascriptOpaque(hooks);
    await hookBoundaries(hooks);
    await hookPhysicalSettlement(hooks);
    await detachedHookOwnership(createHost);
    passed.push("portable hooks all strategies, managed terminals and JavaScript opaque identity");
  } finally {
    await hooks.close();
  }
  await browserOptionsAcceptance();
  passed.push(
    "initialized WASM injection, loader injection, Rust consumer linger and state:false readiness",
  );
  await browserModuleAcceptance();
  passed.push("native modules, URL reload, import maps, CORS and Worker continuity");
  await browserManagerAcceptance();
  await browserManagerLiveness();
  passed.push("Worker HostManager, real RPC references, state, replacement and failure cleanup");
  return passed;
}
