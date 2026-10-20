import { Type, defineFacet, hostNamed, local, method, remote } from "@tembra/sdk";
import { createHost, createHostManager, webWorkerDriver } from "@tembra/sdk/browser";

import { refuseFirstStop } from "../shared/managed-stop.ts";

const Counter = remote("test.worker.manager.counter", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("managed-worker"),
  variants: {
    main: {
      state: { count: Type.Integer() },
      methods: { increment: method(undefined, Type.Integer()), hang: method(), crash: method() },
    },
  },
});
const Flush = remote("test.worker.manager.parent-flush", {
  version: 1,
  input: Type.Object({}),
  host: hostNamed("worker-manager-parent"),
  variants: { main: { methods: { flush: method() } } },
});
function check(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(message);
  }
}
export async function browserManagerAcceptance(): Promise<void> {
  const host = await createHost({ name: "worker-manager-parent" });
  const manager = createHostManager({
    host,
    name: "managed-worker",
    driver: webWorkerDriver({
      module: "/manager-facet.js",
      workerURL: "/manager-worker.js",
      wasmURL: "/wasm/tembra_browser_wasm_bg.wasm",
    }),
    startupTimeout: 10_000,
  });
  let flushes = 0;
  try {
    await host.install(
      defineFacet({
        name: "worker-parent-flush",
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
    const first = await manager.start();
    check(first.status() === "ready", "Worker readiness handshake");
    const counter = await host.get(Counter, {});
    check((await counter.increment()) === 1, "Worker core RPC invocation");
    check(counter.count.value === 1, "Worker state replication");
    counter.release();
    const second = await manager.replace();
    await first.closed;
    check(first.status() === "stopped" && second.generation === 2, "Worker replacement");
    check(flushes === 1, "graceful Worker cleanup can invoke its held parent dependency");
    const next = await host.get(Counter, {});
    check(
      next.count.value === 0 && (await next.increment()) === 1,
      "replacement has its own Host state",
    );
    check(second.activity().receivedFrames > 0, "Worker activity reporting");
    const pendingRejected = next.hang().then(
      () => false,
      () => true,
    );
    await next.crash();
    await second.closed;
    check(second.status() === "failed", "Worker runtime error detection");
    check(await pendingRejected, "Worker disconnect rejects admitted invocation");
    check(next.status === "loading", "disconnected reference waits for a replacement route");
    const waitingRejected = next.increment().then(
      () => false,
      () => true,
    );
    next.release();
    check(await waitingRejected, "release cancels a call waiting for reconnection");
    const third = await manager.start();
    check(third.generation === 3, "restart waits for failed environment cleanup");
    const Owner = local("test.worker.manager.parent-owner", {
      version: 1,
      input: Type.Object({}),
      members: {},
    });
    let independentClosed = false;
    await host.install(
      defineFacet({
        name: "worker-parent-owner",
        uses: [Counter],
        setup(scope, declaration) {
          declaration.serve(
            Owner,
            (_input, self) => {
              const child = self.get(Counter, {});
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
    let refusedCleanup = false;
    try {
      await host.close();
    } catch (error) {
      refusedCleanup =
        error instanceof Error &&
        "reason" in error &&
        error.reason === "cleanup" &&
        "detailsJson" in error &&
        typeof error.detailsJson === "string" &&
        (error.detailsJson.includes("host_closed") ||
          (error.detailsJson.includes("cleanup_dependency_disconnected") &&
            error.detailsJson.includes("managed-worker")));
    }
    check(
      refusedCleanup,
      "parent cleanup preserves the ended child error without waiting for reconnection",
    );
    check(independentClosed, "independent parent cleanup is still attempted");
    check(third.status() === "stopped", "Host close owns Worker cleanup");
    check(flushes === 2, "parent close keeps child cleanup RPC usable until settlement");
  } finally {
    await manager.stop();
    await host.close().catch(() => undefined);
  }
  const disconnectedHost = await createHost({ name: "worker-manager-parent" });
  const disconnectedManager = createHostManager({
    host: disconnectedHost,
    name: "managed-worker",
    driver: webWorkerDriver({
      module: "/manager-facet.js",
      workerURL: "/manager-worker.js",
      wasmURL: "/wasm/tembra_browser_wasm_bg.wasm",
    }),
    startupTimeout: 10_000,
  });
  try {
    await disconnectedHost.install(
      defineFacet({
        name: "worker-parent-flush",
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
    const instance = await disconnectedManager.start();
    const Owner = local("test.worker.manager.disconnected-owner", {
      version: 1,
      input: Type.Object({}),
      members: {},
    });
    let independentClosed = false;
    await disconnectedHost.install(
      defineFacet({
        name: "worker-disconnected-owner",
        uses: [Counter],
        setup(scope, declaration) {
          declaration.serve(
            Owner,
            (_input, self) => {
              const child = self.get(Counter, {});
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
    const child = await disconnectedHost.get(Counter, {});
    await child.crash();
    await instance.closed;
    check(child.status === "loading", "abrupt disconnect preserves ordinary reconnection status");
    child.release();
    let refusedDisconnected = false;
    try {
      await disconnectedHost.close();
    } catch (error) {
      refusedDisconnected =
        error instanceof Error &&
        "reason" in error &&
        error.reason === "cleanup" &&
        "detailsJson" in error &&
        typeof error.detailsJson === "string" &&
        error.detailsJson.includes("cleanup_dependency_disconnected") &&
        error.detailsJson.includes("managed-worker");
    }
    check(refusedDisconnected, "closing refuses a disconnected dependency with its target");
    check(independentClosed, "disconnected dependency does not suppress other resource closers");
  } finally {
    await disconnectedManager.stop();
    await disconnectedHost.close().catch(() => undefined);
  }
  const failureHost = await createHost({ name: "worker-failure-parent" });
  const failed = createHostManager({
    host: failureHost,
    name: "worker-missing",
    driver: webWorkerDriver({
      module: "/missing-module.js",
      workerURL: "/manager-worker.js",
      wasmURL: "/wasm/tembra_browser_wasm_bg.wasm",
    }),
    startupTimeout: 2000,
  });
  try {
    let rejected = false;
    try {
      await failed.start();
    } catch {
      rejected = true;
    }
    check(rejected, "Worker startup failure");
  } finally {
    await failed.stop();
    await failureHost.close();
  }
  for (const outcome of ["timeout", "failure"] as const) {
    // oxlint-disable-next-line no-await-in-loop -- Each Worker generation must finish before the next refusal.
    await stopRefusalAcceptance(outcome);
  }
}

async function stopRefusalAcceptance(outcome: "timeout" | "failure"): Promise<void> {
  const host = await createHost({ name: "worker-manager-parent" });
  const controlled = refuseFirstStop(
    webWorkerDriver({
      module: "/manager-facet.js",
      workerURL: "/manager-worker.js",
      wasmURL: "/wasm/tembra_browser_wasm_bg.wasm",
    }),
    outcome,
  );
  const manager = createHostManager({
    host,
    name: "managed-worker",
    driver: controlled.driver,
    stopTimeout: 1000,
  });
  let flushes = 0;
  try {
    await host.install(
      defineFacet({
        name: "worker-parent-flush",
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
    const first = await manager.start();
    const counter = await host.get(Counter, {});
    check((await counter.increment()) === 1, "first Worker has managed state");
    counter.release();
    let refused = false;
    try {
      await manager.stop();
    } catch (error) {
      refused =
        error instanceof Error &&
        (outcome === "timeout"
          ? "reason" in error && error.reason === "host_stop_timeout"
          : error.message === "stop acknowledgment rejected");
    }
    check(refused, `Worker stop reports ${outcome}`);
    check(manager.current === undefined, "failed stop clears the current instance");
    await first.closed;
    check(first.status() === "stopped", "failed stop settles the instance");
    check(controlled.activity.terminations === 1, "failed stop terminates its Worker once");
    check(controlled.activity.acknowledgments > 0, "the actual child reached its stop receipt");
    check(flushes === 1, "failed acknowledgment preserves graceful resource cleanup");

    const second = await manager.replace();
    check(second.generation === 2, "replace can start after a failed stop");
    const fresh = await host.get(Counter, {});
    check(fresh.count.value === 0, "fresh acquisition belongs to the replacement Host");
    check((await fresh.increment()) === 1, "replacement Worker remains callable");
    fresh.release();
    await manager.stop();
    await second.closed;
    check(manager.current === undefined, "successful stop also clears current");
    check(controlled.activity.terminations === 2, "replacement termination is owned once");
    check(flushes === 2, "replacement cleanup retains its parent RPC route");
    await host.close();
    check(host.diagnostics().jobs === 0, "parent closes all physical jobs");
    check(host.diagnostics().callbacks === 0, "parent releases callbacks");
  } finally {
    await manager.stop();
    await host.close();
  }
}
