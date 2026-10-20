import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { test } from "node:test";
import { setTimeout as delay } from "node:timers/promises";

import { Type, defineFacet, hostNamed, local, method, remote } from "@tembra/sdk";
import { createHost, createHostManager, forkDriver } from "@tembra/sdk/node";
import type { HostDriver } from "@tembra/sdk/node";

import { refuseFirstStop } from "../shared/managed-stop.ts";

type ManagedChannel = Awaited<ReturnType<HostDriver["spawn"]>>;

const Counter = remote("test.manager.counter", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("managed-child"),
  variants: {
    main: {
      state: { count: Type.Integer() },
      methods: { increment: method(undefined, Type.Integer()), crash: method(), hang: method() },
    },
  },
});
async function fixture() {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-manager-"));
  const cleanup = path.join(root, "closed.txt");
  const module = path.join(root, "facet.ts");
  await writeFile(
    module,
    `import {Type,remote,hostNamed,method,defineFacet} from '@tembra/sdk';
import {appendFileSync} from 'node:fs';
const Counter=remote('test.manager.counter',{version:1,input:Type.Object({}),host:hostNamed('managed-child'),variants:{main:{state:{count:Type.Integer()},methods:{increment:method(undefined,Type.Integer()),crash:method(),hang:method()}}}});
export default defineFacet({name:'managed-counter',uses:[],setup(_scope,d){d.serve(Counter,(_input,self)=>{const count=self.cell('count',{schema:Type.Integer(),initial:0});self.resource('cleanup',{open:()=>({}),close:()=>appendFileSync(${JSON.stringify(cleanup)},'closed\\n')});return {variant:'main',state:{count},methods:{increment:ctx=>count.replace(ctx,count.value+1).value,crash(){setTimeout(()=>process.exit(23),10);},async hang(ctx){await ctx.signal.cancelled;}}};});}});`,
  );
  return { root, module, cleanup };
}

for (const outcome of ["timeout", "failure"] as const) {
  // oxlint-disable-next-line no-await-in-loop -- Each child process must finish before the next transport refusal.
  await test(`manager stop ${outcome} clears current and permits a fresh replacement`, async () => {
    const files = await fixture();
    const host = await createHost({ name: "stop-refusal-parent" });
    const controlled = refuseFirstStop(forkDriver({ module: files.module }), outcome);
    const manager = createHostManager({
      host,
      name: "managed-child",
      driver: controlled.driver,
      stopTimeout: 1000,
    });
    try {
      const first = await manager.start();
      const counter = await host.get(Counter, {});
      assert.equal(await counter.increment(), 1);
      counter.release();
      await assert.rejects(
        manager.stop(),
        outcome === "timeout" ? { reason: "host_stop_timeout" } : /stop acknowledgment rejected/u,
      );
      assert.equal(manager.current, undefined);
      await first.closed;
      assert.equal(first.status(), "stopped");
      assert.equal(controlled.activity.terminations, 1);
      assert.ok(controlled.activity.acknowledgments > 0);
      const firstCleanup = await readFile(files.cleanup, "utf-8");
      assert.equal(firstCleanup.trim(), "closed");
      assert.equal(host.diagnostics().handlesByKind.rpc ?? 0, 0);

      const second = await manager.replace();
      assert.equal(second.generation, 2);
      const fresh = await host.get(Counter, {});
      assert.equal(fresh.count.value, 0);
      assert.equal(await fresh.increment(), 1);
      fresh.release();
      await manager.stop();
      await second.closed;
      assert.equal(manager.current, undefined);
      assert.equal(controlled.activity.terminations, 2);
      const finalCleanup = await readFile(files.cleanup, "utf-8");
      assert.equal(finalCleanup.trim().split("\n").length, 2);
      await host.close();
      assert.equal(host.diagnostics().bridge.jobs, 0);
      assert.equal(host.diagnostics().bridge.callbacks, 0);
    } finally {
      await manager.stop();
      await host.close();
      await rm(files.root, { recursive: true, force: true });
    }
  });
}

await test(
  "Node HostManager invokes the real child Rust Host, releases references, replaces, and cleans up",
  { timeout: 30_000 },
  async () => {
    const files = await fixture();
    const host = await createHost({ name: "manager-parent" });
    const manager = createHostManager({
      host,
      name: "managed-child",
      driver: forkDriver({ module: files.module }),
      startupTimeout: 10_000,
    });
    try {
      const first = await manager.start();
      assert.equal(first.status(), "ready");
      assert.equal(await manager.start(), first);
      const counter = await host.get(Counter, {});
      assert.equal(await counter.increment(), 1);
      assert.equal(counter.count.value, 1);
      counter.release();
      const second = await manager.replace();
      assert.equal(first.status(), "stopped");
      await first.closed;
      assert.equal(second.generation, 2);
      const firstCleanup = await readFile(files.cleanup, "utf-8");
      assert.equal(firstCleanup.trim(), "closed");
      const fresh = await host.get(Counter, {});
      assert.equal(fresh.count.value, 0);
      assert.equal(await fresh.increment(), 1);
      fresh.release();
      assert.ok(second.activity().sentFrames > 0);
      assert.ok(second.activity().receivedFrames > 0);
      const closing = host.close();
      await assert.rejects(manager.start(), /closing/u);
      await closing;
      assert.equal(second.status(), "stopped");
      const finalCleanup = await readFile(files.cleanup, "utf-8");
      assert.equal(finalCleanup.trim().split("\n").length, 2);
    } finally {
      await manager.stop();
      await host.close();
      await rm(files.root, { recursive: true, force: true });
    }
  },
);

await test(
  "Node HostManager detects process failure and cleans up failed startup",
  { timeout: 30_000 },
  async () => {
    const files = await fixture();
    const host = await createHost({ name: "failure-parent" });
    const manager = createHostManager({
      host,
      name: "managed-child",
      driver: forkDriver({ module: files.module }),
    });
    try {
      const instance = await manager.start();
      const counter = await host.get(Counter, {});
      const pendingRejected = assert.rejects(counter.hang());
      await counter.crash();
      await instance.closed;
      await pendingRejected;
      const waitingRejected = assert.rejects(counter.increment());
      assert.equal(instance.status(), "failed");
      counter.release();
      await waitingRejected;
      await manager.stop();
      const failed = createHostManager({
        host,
        name: "missing-child",
        driver: forkDriver({ module: path.join(files.root, "missing.ts") }),
        startupTimeout: 2000,
      });
      await assert.rejects(failed.start());
      await failed.stop();
      await host.close();
      const diagnostics = host.diagnostics();
      assert.equal(diagnostics.bridge.callbacks, 0);
      assert.equal(diagnostics.bridge.values, 0);
      assert.equal(diagnostics.bridge.jobs, 0);
    } finally {
      await manager.stop();
      await host.close();
      await rm(files.root, { recursive: true, force: true });
    }
  },
);

await test("eager child dependencies and cleanup calls retain their parent RPC route", async () => {
  const files = await fixture();
  const host = await createHost({ name: "dependency-parent" });
  const Flush = remote("test.manager.parent-flush", {
    version: 1,
    input: Type.Object({}),
    host: hostNamed("dependency-parent"),
    variants: { main: { methods: { flush: method() } } },
  });
  let flushes = 0;
  const manager = createHostManager({
    host,
    name: "managed-child",
    driver: forkDriver({ module: files.module }),
    startupTimeout: 3000,
    stopTimeout: 2000,
  });
  try {
    await host.install(
      defineFacet({
        name: "parent-flush",
        uses: [],
        setup(_scope, declaration) {
          declaration.serve(Flush, () => ({
            variant: "main",
            state: {},
            methods: {
              flush() {
                flushes += 1;
                return undefined;
              },
            },
          }));
        },
      }),
    );
    await writeFile(
      files.module,
      `
      import {Type,defineFacet,local,remote,hostNamed,method} from '@tembra/sdk';
      const Flush=remote('test.manager.parent-flush',{version:1,input:Type.Object({}),host:hostNamed('dependency-parent'),variants:{main:{methods:{flush:method()}}}});
      const Owner=local('test.manager.eager-owner',{version:1,input:Type.Object({}),members:{}});
      export default defineFacet({name:'eager-child',uses:[Flush],setup(scope,declaration){
        declaration.serve(Owner,(_input,self)=>{
          const parent=self.get(Flush,{});
          self.resource('flush-on-close',{deps:[parent],open:()=>({}),async close(){
            await scope.root(async context=>{await parent.flush(context);});
          }});
          return {state:{},methods:{}};
        },{lifetime:'host'});
      }});
    `,
    );
    const instance = await manager.start();
    assert.equal(instance.status(), "ready");
    await manager.stop();
    assert.equal(instance.status(), "stopped");
    assert.equal(flushes, 1);
  } finally {
    await manager.stop();
    await host.close();
    await rm(files.root, { recursive: true, force: true });
  }
});

await test("startup deadline covers stalled spawn and terminates a channel that arrives after parent close", async () => {
  const host = await createHost({ name: "stalled-parent" });
  let resolveChannel!: (channel: ManagedChannel) => void;
  let spawns = 0;
  let terminations = 0;
  const manager = createHostManager({
    host,
    name: "stalled-child",
    startupTimeout: 30,
    driver: {
      async spawn() {
        spawns += 1;
        // oxlint-disable-next-line promise/avoid-new -- Model an asynchronous environment constructor finishing after its startup deadline.
        return await new Promise<ManagedChannel>((resolve) => {
          resolveChannel = resolve;
        });
      },
    },
  });
  const refused = assert.rejects(manager.start(), { reason: "host_start_timeout" });
  const closing = host.close();
  await refused;
  await closing;
  resolveChannel({
    send() {
      /* The timed-out channel never receives initialization. */
    },
    onMessage: () => () => undefined,
    onFailure: () => () => undefined,
    async terminate() {
      await delay(0);
      terminations += 1;
    },
  });
  await delay(10);
  assert.equal(terminations, 1);
  await assert.rejects(manager.start(), /closing/u);
  assert.equal(spawns, 1);
  assert.equal(host.diagnostics().bridge.jobs, 0);
});

for (const disconnected of [false, true]) {
  // oxlint-disable-next-line no-await-in-loop -- Each native child must finish cleanup before the next case starts.
  await test(
    `parent cleanup refuses a ${disconnected ? "disconnected" : "gracefully ended"} child and attempts other closers`,
    { timeout: 5000 },
    async () => {
      const files = await fixture();
      const host = await createHost({ name: "dependent-parent" });
      const manager = createHostManager({
        host,
        name: "managed-child",
        driver: forkDriver({ module: files.module }),
      });
      const Owner = local("test.manager.parent-owner", {
        version: 1,
        input: Type.Object({}),
        members: {},
      });
      let independentClosed = false;
      let dependencyStatus: (() => string) | undefined;
      try {
        const instance = await manager.start();
        await host.install(
          defineFacet({
            name: "parent-dependent-owner",
            uses: [Counter],
            setup(scope, declaration) {
              declaration.serve(
                Owner,
                (_input, self) => {
                  const child = self.get(Counter, {});
                  dependencyStatus = () => child.status;
                  self.resource("child-dependent", {
                    deps: [child],
                    open: () => ({}),
                    async close() {
                      await scope.root(async (context) => {
                        await child.increment(context);
                      });
                    },
                  });
                  self.resource("independent", {
                    open: () => ({}),
                    close() {
                      independentClosed = true;
                    },
                  });
                  return { state: {}, methods: {} };
                },
                { lifetime: "host" },
              );
            },
          }),
        );
        if (disconnected) {
          const child = await host.get(Counter, {});
          await child.crash();
          await instance.closed;
          assert.equal(child.status, "loading");
          child.release();
        } else {
          await manager.stop();
        }
        assert.ok(dependencyStatus, "eager factory installs a dependency observer");
        const expectedDisconnected = dependencyStatus() === "loading";
        assert.ok(expectedDisconnected || dependencyStatus() === "gone");
        await assert.rejects(host.close(), (error) => {
          assert.ok(
            error instanceof Error && "reason" in error && error.reason === "cleanup",
            String(error),
          );
          assert.ok("detailsJson" in error && typeof error.detailsJson === "string");
          const details: unknown = JSON.parse(error.detailsJson);
          const encoded = JSON.stringify(details);
          if (expectedDisconnected) {
            assert.match(encoded, /cleanup_dependency_disconnected/u);
            assert.match(encoded, /managed-child/u);
          } else {
            assert.match(encoded, /host_closed/u);
          }
          return true;
        });
        assert.equal(independentClosed, true);
        assert.equal(host.diagnostics().bridge.callbacks, 0);
        assert.equal(host.diagnostics().bridge.jobs, 0);
      } finally {
        await manager.stop();
        await host.close().catch(() => undefined);
        await rm(files.root, { recursive: true, force: true });
      }
    },
  );
}
