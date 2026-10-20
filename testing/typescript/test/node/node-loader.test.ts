import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Type, local, method } from "@tembra/sdk";
import type { EsmArtifactV1 } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { deferredVoid, defined, reason, structuredError } from "@tembra/testing";

const Service = local("test.loader.service", {
  version: 1,
  input: Type.Object({}),
  members: { run: method(undefined, Type.Integer()) },
});
const token = JSON.stringify(Service.descriptor);
type MutableArtifact = { -readonly [Key in keyof EsmArtifactV1]: EsmArtifactV1[Key] };
function artifact(source: string, exports = ["default"]): MutableArtifact {
  return {
    format: "tembra-esm-v1",
    identity: "loader-test",
    source,
    exports,
    sha256: createHash("sha256").update(source).digest("hex"),
  };
}
function counter(step = 1) {
  return artifact(`let calls=0; export default {name:'loader-service',uses:[],setup(s,d){
    d.serve({descriptor:${token}},()=>({state:{},methods:{run(){calls+=${step};return calls}}}));
  }};`);
}

await test("fresh module globals across Hosts and reload generations", async () => {
  const one = await createHost();
  const two = await createHost();
  const compiled = counter();
  try {
    const before = one.diagnostics().loader.evaluations;
    await one.load(compiled);
    await two.load(compiled);
    const a = await one.get(Service, {});
    const b = await two.get(Service, {});
    try {
      assert.equal(await a.run(), 1);
      assert.equal(await a.run(), 2);
      assert.equal(await b.run(), 1);
      await one.reload([compiled]);
      assert.equal(await a.run(), 1);
      assert.equal(one.diagnostics().loader.evaluations, before + 3);
      assert.equal(one.diagnostics().codeVersion, 2);
      assert.equal(one.diagnostics().loader.physicalUnloadSupported, false);
    } finally {
      a.release();
      b.release();
    }
  } finally {
    await one.close();
    await two.close();
  }
});

await test("validation snapshots input and rejects an entire batch before evaluation", async () => {
  const host = await createHost();
  try {
    const before = host.diagnostics().loader.evaluations;
    const observable = artifact(
      "globalThis.__tembraLoaderExecuted=true;export default {name:'side-effect',uses:[],setup(){}};",
    );
    await assert.rejects(
      host.reload([observable, { ...counter(), sha256: "0".repeat(64) }]),
      reason("artifact_hash"),
    );
    assert.equal(globalThis.__tembraLoaderExecuted, undefined);
    await assert.rejects(
      host.reload([{ ...observable, sha256: "0".repeat(64), setup() {} }]),
      reason("portable"),
    );
    assert.equal(globalThis.__tembraLoaderExecuted, undefined);
    for (const source of [
      "import './dependency.js';export default {};",
      "export default ()=>import('node:fs');",
      "export default import.meta;",
    ]) {
      // oxlint-disable-next-line no-await-in-loop -- Every refusal releases admission before the next attempt.
      await assert.rejects(host.load(artifact(source)), reason("artifact_import"));
    }
    assert.equal(host.diagnostics().loader.evaluations, before);
    const mutable = counter();
    const pending = host.load(mutable);
    mutable.source = "import('outside');export default {};";
    mutable.sha256 = "0".repeat(64);
    await pending;
    const reference = await host.get(Service, {});
    assert.equal(await reference.run(), 1);
    reference.release();
  } finally {
    Reflect.deleteProperty(globalThis, "__tembraLoaderExecuted");
    await host.close();
  }
});

await test("multiple exports share one evaluated module per batch", async () => {
  const host = await createHost();
  const compiled = artifact(
    `globalThis.__tembraLoaderBatch=(globalThis.__tembraLoaderBatch??0)+1;
    export const first={name:'first-export',uses:[],setup(){}};
    export const second={name:'second-export',uses:[],setup(){}};`,
    ["first", "second"],
  );
  try {
    const before = host.diagnostics().loader.evaluations;
    await host.reload([
      { artifact: compiled, selected: "first" },
      { artifact: compiled, selected: "second" },
    ]);
    assert.equal(globalThis.__tembraLoaderBatch, 1);
    assert.equal(host.diagnostics().loader.evaluations, before + 1);
  } finally {
    Reflect.deleteProperty(globalThis, "__tembraLoaderBatch");
    await host.close();
  }
});

await test("failed imports and candidates consume budget and release callbacks", async () => {
  const host = await createHost();
  try {
    await host.load(counter());
    const reference = await host.get(Service, {});
    const before = host.diagnostics();
    await assert.rejects(
      host.reload([artifact("throw new Error('evaluation fixture');export default {};")]),
      reason("artifact_evaluation"),
    );
    await assert.rejects(
      host.load(artifact("export default {name:'',uses:[],setup(){}};")),
      reason("facet_name"),
    );
    await assert.rejects(
      host.reload([
        artifact(
          "export default {name:'loader-service',uses:[],setup(){throw new Error('setup fixture')}};",
        ),
      ]),
    );
    await delay(5);
    const after = host.diagnostics();
    assert.equal(after.loader.evaluations, before.loader.evaluations + 3);
    assert.equal(after.loader.evaluationFailures, before.loader.evaluationFailures + 1);
    assert.equal(after.codeVersion, before.codeVersion);
    assert.equal(after.bridge.callbacks, before.bridge.callbacks);
    assert.equal(await reference.run(), 1);
    reference.release();
  } finally {
    await host.close();
  }
});

await test("top-level await blocks admission and close joins physical evaluation", async () => {
  const host = await createHost();
  const other = await createHost();
  const { promise: entered, resolve: enter } = deferredVoid();
  const { promise: gate, resolve: finish } = deferredVoid();
  globalThis.__tembraLoaderGate = { enter, gate };
  try {
    const pending = host.load(
      artifact(
        "const p=globalThis.__tembraLoaderGate;p.enter();await p.gate;export default {name:'pending',uses:[],setup(){}};",
      ),
    );
    const rejected = assert.rejects(pending, /closed/u);
    await entered;
    assert.equal(defined(host.diagnostics().loader.inFlight ?? undefined).stage, "evaluating");
    await assert.rejects(other.load(counter()), reason("loader_busy"));
    let closed = false;
    const closing = host.close().then(() => {
      closed = true;
    });
    await delay(5);
    assert.equal(closed, false);
    assert.equal(defined(host.diagnostics().loader.inFlight ?? undefined).stage, "evaluating");
    await assert.rejects(other.load(counter()), reason("loader_busy"));
    finish();
    await rejected;
    await closing;
    assert.equal(host.diagnostics().loader.inFlight, null);
    await other.load(counter());
  } finally {
    finish();
    Reflect.deleteProperty(globalThis, "__tembraLoaderGate");
    await host.close();
    await other.close();
  }
});

await test(
  "close cancels eager compiled startup before joining its physical resource callback",
  { timeout: 3000 },
  async () => {
    const host = await createHost();
    const { promise: entered, resolve: enter } = deferredVoid();
    globalThis.__tembraLoaderStartup = { enter, closed: 0 };
    try {
      const loading = host.load(
        artifact(`export default {name:'startup',uses:[],setup(scope,d){
      d.serve({descriptor:${token}},(_i,self)=>{
        self.resource('startup',{async open(){
          globalThis.__tembraLoaderStartup.enter();
          await scope.signal.cancelled;
          return 1;
        },close(){globalThis.__tembraLoaderStartup.closed++;}});
        return {state:{},methods:{run(){return 1}}};
      },{lifetime:'host'});
    }};`),
      );
      const refusal = assert.rejects(loading);
      await entered;
      await host.close();
      await refusal;
      assert.equal(globalThis.__tembraLoaderStartup.closed, 1);
      assert.equal(host.diagnostics().bridge.callbacks, 0);
      assert.equal(host.diagnostics().loader.inFlight, null);
    } finally {
      Reflect.deleteProperty(globalThis, "__tembraLoaderStartup");
      await host.close();
    }
  },
);

await test("compiled physical callbacks remain owned through shutdown", async () => {
  const host = await createHost();
  const { promise: entered, resolve: enter } = deferredVoid();
  const { promise: gate, resolve: finish } = deferredVoid();
  globalThis.__tembraLoaderGate = { enter, gate };
  try {
    await host.load(
      artifact(`export default {name:'loader-service',uses:[],setup(s,d){d.serve({descriptor:${token}},()=>({state:{},methods:{async run(){
      const p=globalThis.__tembraLoaderGate;p.enter();await p.gate;return 7;
    }}}));}};`),
    );
    const reference = await host.get(Service, {});
    const call = (async () => {
      try {
        return await reference.run();
      } catch (error) {
        return error;
      }
    })();
    await entered;
    const old = host.diagnostics().generations.find((generation) => generation.kind === "esm");
    assert.ok(old);
    assert.equal(old.physicalCallbacks, 1);
    reference.release();
    const closing = host.close();
    await delay(5);
    assert.equal(
      defined(host.diagnostics().generations.find((generation) => generation.id === old.id))
        .physicalCallbacks,
      1,
    );
    finish();
    await call;
    await closing;
    assert.equal(host.diagnostics().bridge.callbacks, 0);
    assert.equal(host.diagnostics().generations.length, 0);
    assert.ok(
      host.diagnostics().releasedGenerations.some((generation) => generation.id === old.id),
    );
  } finally {
    finish();
    Reflect.deleteProperty(globalThis, "__tembraLoaderGate");
    await host.close();
  }
});

await test("old generation callbacks retain their owners while core reload is pending", async () => {
  const host = await createHost();
  const { promise: entered, resolve: enter } = deferredVoid();
  const { promise: gate, resolve: finish } = deferredVoid();
  globalThis.__tembraLoaderGate = { enter, gate };
  try {
    await host.load(
      artifact(`export default {name:'loader-service',uses:[],setup(s,d){
      d.serve({descriptor:${token}},()=>({state:{},methods:{async run(){
        const gate=globalThis.__tembraLoaderGate;gate.enter();await gate.gate;return 7;
      }}}));}};`),
    );
    const reference = await host.get(Service, {});
    const call = (async () => {
      try {
        return await reference.run();
      } catch (error) {
        return error;
      }
    })();
    await entered;
    const old = host.diagnostics().generations.find((generation) => generation.kind === "esm");
    assert.ok(old);
    const reload = host.reload([counter(2)]);
    await delay(5);
    const retained = host.diagnostics().generations.find((generation) => generation.id === old.id);
    assert.ok(retained);
    assert.equal(retained.physicalCallbacks, 1);
    assert.ok(retained.callbacks > 0);
    finish();
    await call;
    await reload;
    assert.equal(await reference.run(), 2);
    await host.reload([counter(3)]);
    await delay(5);
    assert.equal(await reference.run(), 3);
    reference.release();
    await host.close();
    assert.ok(
      host.diagnostics().releasedGenerations.some((generation) => generation.id === old.id),
    );
  } finally {
    finish();
    Reflect.deleteProperty(globalThis, "__tembraLoaderGate");
    await host.close();
  }
});

await test("lifetime budget survives Host churn and refuses without changing the Host", async () => {
  let host = await createHost();
  try {
    while (host.diagnostics().loader.remainingEvaluations > 1) {
      // oxlint-disable-next-line no-await-in-loop -- Each failed selection consumes a distinct cache identity.
      await assert.rejects(host.load(artifact("export default {};")), reason("facet"));
    }
    await host.close();
    host = await createHost();
    assert.equal(host.diagnostics().loader.evaluations, 31);
    await assert.rejects(
      host.reload([
        artifact(
          "globalThis.__tembraLoaderOverBudget=true;export default {name:'over-budget',uses:[],setup(){}};",
        ),
        counter(),
      ]),
      (error) => {
        structuredError(error);
        assert.equal(error.reason, "loader_budget_exhausted");
        assert.equal(error.details.requested, 2);
        assert.equal(error.details.codeVersion, 1);
        return true;
      },
    );
    assert.equal(globalThis.__tembraLoaderOverBudget, undefined);
    await host.load(counter());
    const reference = await host.get(Service, {});
    assert.equal(await reference.run(), 1);
    const before = host.diagnostics();
    for (let i = 0; i < 100; i += 1) {
      // oxlint-disable-next-line no-await-in-loop -- Refusal must not create imports, callbacks or core versions.
      await assert.rejects(host.reload([counter(2)]), reason("loader_budget_exhausted"));
    }
    const after = host.diagnostics();
    assert.equal(after.loader.evaluations, 32);
    assert.equal(after.loader.sourceBytes, before.loader.sourceBytes);
    assert.equal(after.codeVersion, before.codeVersion);
    assert.deepEqual(after.bridge, before.bridge);
    assert.equal(after.loader.requiresProcessRotation, true);
    assert.equal(after.loader.inFlight, null);
    assert.equal(await reference.run(), 2);
    reference.release();
  } finally {
    Reflect.deleteProperty(globalThis, "__tembraLoaderOverBudget");
    await host.close();
  }
});
