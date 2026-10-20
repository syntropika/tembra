import { Type, defineFacet, local, method } from "@tembra/sdk";

import { step } from "./browser-module-dependency.js";

declare global {
  var __browserModuleState: { closed: number } | undefined;
  var __browserModuleEntered: boolean | undefined;
  var __browserModuleGate: Promise<void> | undefined;
}
globalThis.__browserModuleState ??= { closed: 0 };
const state = globalThis.__browserModuleState;
const Counter = local("test.browser.module-counter", {
  version: 1,
  input: Type.Object({}),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
  },
});
export const namedFacet = defineFacet({
  name: "browser-module-counter",
  uses: [],
  setup(_scope, declaration) {
    declaration.serve(Counter, (_input, self) => {
      const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
      self.resource("cleanup", {
        open: () => ({}),
        close() {
          state.closed += 1;
        },
      });
      return {
        state: { count },
        methods: {
          increment(context) {
            return count.replace(context, count.value + step).value;
          },
        },
      };
    });
  },
});
export default namedFacet;
