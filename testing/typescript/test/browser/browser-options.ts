import { Type, defineFacet, local } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/browser";
import type { BrowserWasmModule } from "@tembra/sdk/browser";

function check(value: boolean, message: string): asserts value {
  if (!value) {
    throw new Error(message);
  }
}

const DeferredState = local("test.browser.options-state", {
  version: 1,
  input: Type.Object({}),
  members: { value: Type.Integer() },
});

/** Exercise public initialization, constructor policy and acquisition against the actual WASM. */
export async function browserOptionsAcceptance(): Promise<void> {
  const url = new URL("/wasm/tembra_browser_wasm.js", location.href).href;
  const namespace: unknown = await import(url);
  check(
    namespace !== null &&
      typeof namespace === "object" &&
      "default" in namespace &&
      typeof namespace.default === "function" &&
      "WasmHost" in namespace &&
      typeof namespace.WasmHost === "function" &&
      "createCheckedWasmExecutor" in namespace &&
      typeof namespace.createCheckedWasmExecutor === "function",
    "the served matching WASM module exposes its initializer and binding",
  );
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This is the actual generated SDK module served with its WASM asset; its observed exports are checked above.
  const module = namespace as BrowserWasmModule & { default: () => Promise<unknown> };
  await module.default();

  const host = await createHost({
    name: "injected-options",
    wasm: module,
    consumerLinger: { types: { [DeferredState.name]: 1000 }, capacity: 2 },
  });
  let release!: () => void;
  let initialized!: () => void;
  // oxlint-disable-next-line promise/avoid-new -- Explicit gates keep a real Rust initializer pending until the test releases it.
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  // oxlint-disable-next-line promise/avoid-new -- Observe initializer entry before inspecting readiness of the same issued reference.
  const entered = new Promise<void>((resolve) => {
    initialized = resolve;
  });
  try {
    await host.install(
      defineFacet({
        name: "options-state",
        uses: [],
        setup(_scope, declaration) {
          declaration.serve(DeferredState, (_input, self) => {
            const value = self.cell("value", {
              schema: Type.Integer(),
              async initialize() {
                initialized();
                await gate;
                return 7;
              },
            });
            return { state: { value }, methods: {} };
          });
        },
      }),
    );
    const reference = host.hold(DeferredState, {}, { state: false });
    let ready = false;
    const readiness = reference.ready.then(() => {
      ready = true;
    });
    try {
      await entered;
      // Shape-only state demand does not bypass the provider's own asynchronous activation.
      const { status } = reference;
      check(!ready && status === "loading", "pending local initialization remains loading");
      release();
      await readiness;
      check(
        reference.status === "live",
        "shape-only acquisition becomes ready on its original claim",
      );
      let pendingState: unknown;
      try {
        void reference.value.value;
      } catch (error) {
        pendingState = error;
      }
      check(
        pendingState instanceof Error &&
          "code" in pendingState &&
          pendingState.code === "not_ready" &&
          "reason" in pendingState &&
          pendingState.reason === "state",
        "shape-only readiness does not imply state hydration",
      );
      const hydrated = await host.get(DeferredState, {}, { state: true });
      try {
        check(hydrated.value.value === 7, "state-demanding acquisition awaits hydration");
        const observed: number[] = [];
        const unsubscribe = hydrated.value.subscribe((value) => {
          observed.push(value);
        });
        try {
          check(observed[0] === 7, "the hydrated claim observes its initial state");
        } finally {
          unsubscribe();
        }
      } finally {
        hydrated.release();
      }
    } finally {
      release();
      reference.release();
      await readiness;
    }
  } finally {
    release();
    await host.close();
  }

  let loaderCalls = 0;
  const asynchronouslyInitialized = await createHost({
    wasm: async () => {
      loaderCalls += 1;
      await module.default();
      return module;
    },
  });
  try {
    check(loaderCalls === 1, "the public async module loader is awaited exactly once");
    check(
      Array.isArray(asynchronouslyInitialized.diagnostics().errors),
      "the async loader produces a usable Rust Host",
    );
  } finally {
    await asynchronouslyInitialized.close();
  }

  let refusal: unknown;
  try {
    const unexpected = await createHost({
      wasm: module,
      consumerLinger: { types: { [DeferredState.name]: -1 }, capacity: 2 },
    });
    await unexpected.close();
  } catch (error) {
    refusal = error;
  }
  check(
    refusal instanceof Error &&
      "code" in refusal &&
      refusal.code === "invalid" &&
      "reason" in refusal &&
      refusal.reason === "consumer_linger",
    "consumerLinger reaches the real Rust constructor and invalid policy is rejected",
  );
}
