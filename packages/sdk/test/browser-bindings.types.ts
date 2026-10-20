import { BrowserExecutor } from "../src/browser/executor.js";
import type { ExecutorWasmModule } from "../src/browser/executor.js";
import type { BrowserClient } from "../src/browser/index.js";
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
async function typeChecks(client: BrowserClient, module: ExecutorWasmModule) {
  const executor = new BrowserExecutor(client, module);
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
  await executor.install(facet);
  // @ts-expect-error: the executor consumes the actual client, not a forged Context
  const invalid = new BrowserExecutor({ runtime: "tembra", handle: {} }, module);
  void invalid;
}
void typeChecks;
