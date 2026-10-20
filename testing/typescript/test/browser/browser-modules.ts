/* oxlint-disable no-await-in-loop -- Native module versions, quota refusals and physical gates are observed sequentially. */
/* oxlint-disable promise/avoid-new -- Worker messages and import gates expose physical completion. */
import { Type, local, method } from "@tembra/sdk";
import type { FacetDefinition } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/browser";

const Counter = local("test.browser.module-counter", {
  version: 1,
  input: Type.Object({}),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
  },
});
function check(value: boolean, message: string): asserts value {
  if (!value) {
    throw new Error(message);
  }
}

export async function workerModuleAcceptance(): Promise<void> {
  const host = await createHost();
  try {
    await host.load("./modules/v1/facet.js");
    const reference = await host.get(Counter, {});
    check((await reference.increment()) === 1, "worker module load");
    await host.reload(["./modules/v2/facet.js"]);
    check(
      (await reference.increment()) === 3 && reference.count.value === 3,
      "worker state/reference continuity",
    );
    reference.release();
  } finally {
    await host.close();
  }
  check(globalThis.__browserModuleState?.closed === 1, "worker resource cleanup");
}

async function importedInstallation(): Promise<void> {
  const moduleURL = new URL("/modules/v1/facet.js", location.href).href;
  const namespace: unknown = await import(moduleURL);
  check(
    namespace !== null && typeof namespace === "object" && "default" in namespace,
    "previously imported facet",
  );
  const installed = await createHost();
  try {
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The real fixture is authored with defineFacet and its export is checked above.
    const facet = namespace.default as FacetDefinition;
    await installed.install(facet);
    const reference = await installed.get(Counter, {});
    check((await reference.increment()) === 1, "installed imported facet");
    await installed.reload([facet]);
    check((await reference.increment()) === 2, "already evaluated facet reload");
    reference.release();
  } finally {
    await installed.close();
  }
}

async function nativeModuleReloads(): Promise<void> {
  const previous = globalThis.__browserModuleState?.closed ?? 0;
  const host = await createHost({ baseURL: new URL("/modules/", location.href) });
  try {
    await host.load("./v1/facet.js");
    const reference = await host.get(Counter, {});
    let observed = 0;
    let remove = reference.count.subscribe((value) => {
      observed = value;
    });
    check((await reference.increment()) === 1, "relative module import with native dependency");
    remove();
    await host.reload([
      { module: new URL("/modules/v2/facet.js", location.href), selected: "namedFacet" },
    ]);
    const afterReload = reference.count.value;
    remove = reference.count.subscribe((value) => {
      observed = value;
    });
    const incremented = await reference.increment();
    check(
      incremented === 3 && reference.count.value === 3 && observed === 3,
      `URL reload continuity: result=${incremented}, state=${reference.count.value}, afterReload=${afterReload}, observed=${observed}`,
    );
    remove();
    for (let version = 0; version < 40; version += 1) {
      await host.reload([`./v2/facet.js?revision=${version}`]);
      check(
        (await reference.increment()) === 5 + version * 2,
        "native module reload exceeds the legacy artifact budget",
      );
    }
    check(
      host.diagnostics().artifactEvaluations === 0,
      "ordinary modules do not consume artifact reservations",
    );
    reference.release();
  } finally {
    await host.close();
  }
  check(globalThis.__browserModuleState?.closed === previous + 1, "module resource cleanup");
}

async function nativeModuleErrors(): Promise<void> {
  const mapped = await createHost();
  try {
    await mapped.load("acceptance-facet");
    const reference = await mapped.get(Counter, {});
    check((await reference.increment()) === 1, "native import map");
    await mapped.reload(["acceptance/facet.js"]);
    check((await reference.increment()) === 2, "bare import-map subpath ending in .js");
    reference.release();
    for (const specifier of ["/missing-module.js", "/invalid-module.js", "http://["]) {
      let refused = false;
      try {
        await mapped.load(specifier);
      } catch (error) {
        refused = error instanceof Error && "reason" in error;
      }
      check(refused, "structured browser URL/module error");
    }
    const foreign = new URL("/modules/v1/facet.js", location.href);
    foreign.hostname = foreign.hostname === "localhost" ? "127.0.0.1" : "localhost";
    let corsRefused = false;
    try {
      await mapped.load(foreign);
    } catch {
      corsRefused = true;
    }
    check(corsRefused, "native CORS refusal");
  } finally {
    await mapped.close();
  }
}

async function workerAndPendingClose(): Promise<void> {
  const worker = new Worker(new URL("/module-worker.js", location.href), { type: "module" });
  try {
    await new Promise<void>((resolve, reject) => {
      worker.addEventListener(
        "message",
        (event) => {
          if (event.data === "passed") {
            resolve();
          } else {
            reject(new Error(String(event.data)));
          }
        },
        { once: true },
      );
      worker.addEventListener(
        "error",
        (event) => {
          reject(new Error(event.message));
        },
        { once: true },
      );
      // oxlint-disable-next-line unicorn/require-post-message-target-origin -- Worker.postMessage accepts transfer options, not a Window targetOrigin.
      worker.postMessage("run");
    });
  } finally {
    worker.terminate();
  }

  let release: (() => void) | undefined;
  globalThis.__browserModuleGate = new Promise<void>((resolve) => {
    release = resolve;
  });
  const pendingHost = await createHost();
  let refused = false;
  const loading = pendingHost.load("/pending-module.js").catch(() => {
    refused = true;
  });
  for (let turn = 0; globalThis.__browserModuleEntered !== true && turn < 100; turn += 1) {
    await new Promise<void>((resolve) => {
      setTimeout(resolve, 5);
    });
  }
  check(globalThis.__browserModuleEntered === true, "physical module evaluation entered");
  let closed = false;
  const closing = pendingHost.close().then(() => {
    closed = true;
  });
  await new Promise<void>((resolve) => {
    setTimeout(resolve, 5);
  });
  check(!closed, "close joins native module evaluation");
  release?.();
  await loading;
  await closing;
  check(refused && closed, "closed Host does not install a late module");
}

async function nativeModuleBudget(): Promise<void> {
  const mappedBudget = await createHost();
  try {
    const { modules } = mappedBudget.diagnostics();
    check(
      modules !== null &&
        typeof modules === "object" &&
        "remainingEvaluations" in modules &&
        typeof modules.remainingEvaluations === "number",
      "native module diagnostics",
    );
    const remaining = modules.remainingEvaluations;
    for (let attempt = 0; attempt < remaining; attempt += 1) {
      try {
        await mappedBudget.load("/missing-module.js");
      } catch {
        // Failed native imports still consume admission.
      }
    }
    let budgetRefused = false;
    try {
      await mappedBudget.load("acceptance-facet");
    } catch (error) {
      budgetRefused =
        error instanceof Error && "reason" in error && error.reason === "loader_budget_exhausted";
    }
    check(budgetRefused, "native module budget cannot reset with Host churn");
  } finally {
    await mappedBudget.close();
  }
}

export async function browserModuleAcceptance(): Promise<void> {
  await importedInstallation();
  await nativeModuleReloads();
  await nativeModuleErrors();
  await workerAndPendingClose();
  await nativeModuleBudget();
}
