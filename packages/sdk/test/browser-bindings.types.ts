import type { BrowserHost, BrowserWasmModule } from "../src/browser/index.js";
import type { WasmExecutorPort } from "../src/host/executor.js";
import { Type, defineFacet, local, method, opaque } from "../src/index.js";
import type { ExplicitCapability, ManagedContext } from "../src/index.js";

const Counter = local("browser.types", {
  version: 1,
  input: Type.Object({ name: Type.String() }),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
    url: opaque<URL>("url@1"),
  },
});
async function typeChecks(host: BrowserHost, incomplete: Omit<WasmExecutorPort, "registerHook">) {
  const facet = defineFacet({
    name: "browser.typed",
    uses: [Counter],
    setup(scope, declaration) {
      declaration.serve(Counter, (input, self) => {
        const label: string = input.name;
        const count = self.cell("count", { schema: Type.Integer(), initial: 2 });
        const derived = self.derive([count], Type.Integer(), ([snapshot]) => snapshot.value * 2);
        const resource = self.resource("url", { open: () => new URL("https://example.com/") });
        const capability: ExplicitCapability<URL> = resource;
        // @ts-expect-error: resource-backed opaque capabilities preserve their semantic value type
        const wrong: ExplicitCapability<string> = resource;
        void [scope, label, derived, capability, wrong];
        return {
          methods: {
            increment(context: ManagedContext) {
              count.change(context, (draft) => {
                draft.value += 1;
              });
              return count.value;
            },
          },
          state: { count },
          opaque: { url: resource },
        };
      });
    },
  });
  await host.install(facet);
  // @ts-expect-error: the private ABI requires portable hook support in every shipped binding.
  const invalid: WasmExecutorPort = incomplete;
  // @ts-expect-error: the SDK requires its exact options-capable WASM constructor, with no older-binding fallback.
  const obsolete: BrowserWasmModule = { WasmHost: {} };
  void [invalid, obsolete];
}
void typeChecks;
