/* oxlint-disable no-eval -- Direct eval deliberately observes the compiled module's lexical environment. */
import {
  Type,
  defineFacet,
  defineHook,
  hookHandler,
  local,
  method,
  native,
  opaque,
  registerHook,
} from "@tembra/sdk";

import { step } from "./dependency.mjs";

export const Bias = local("test.compiled-loader.bias", {
  version: 1,
  input: Type.Object({}),
  members: { read: method(undefined, Type.Integer()) },
});
export const Hook = defineHook("test.compiled-loader.hook", {
  version: 1,
  strategy: "waterfall",
  input: Type.Integer(),
});
const Contributor = local("test.compiled-loader.contributor", {
  version: 1,
  input: Type.Object({}),
  members: { run: Hook.method },
});
export const Capability = native("test.compiled-loader.capability@1");
export const Counter = local("test.compiled-loader.counter", {
  version: 1,
  input: Type.Object({}),
  members: {
    count: Type.Integer(),
    increment: method(undefined, Type.Integer()),
    hook: method(Type.Integer(), Type.Integer()),
    observations: method(undefined, Type.String()),
    wait: method(undefined, Type.Integer()),
    value: opaque(Capability.native),
  },
});

// The compiler must preserve direct eval at module scope, including computed source.
const observations = {
  arguments: eval("typeof arguments"),
  arrowArguments: (() => eval("typeof arguments"))(),
  require: eval("typeof require"),
  module: eval("typeof module"),
  exports: eval("typeof exports"),
  filename: eval("typeof __filename"),
  dirname: eval("typeof __dirname"),
  jitiImport: eval("typeof jitiImport"),
  jitiESMResolve: eval("typeof jitiESMResolve"),
};
const entryPayload = Array.from({ length: 50_000 }, () => step);
const read = () => entryPayload[0];
globalThis.__tembraCompiledLoaderTest.observe?.(read);

const facet = defineFacet({
  name: "compiled-loader-fixture",
  uses: [Bias],
  setup(scope, declaration) {
    declaration.serve(Counter, (_input, self) => {
      const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
      const bias = self.get(Bias, {});
      const value = self.resource("capability", {
        open: () => globalThis.__tembraCompiledLoaderTest.capability,
        close() {
          globalThis.__tembraCompiledLoaderTest.closed += 1;
        },
      });
      return {
        state: { count },
        opaque: { value },
        methods: {
          async increment(context) {
            count.replace(context, count.value + read());
            return count.value + (await bias.read(context));
          },
          async hook(_context, input) {
            return await scope.root((context) => scope.runHook(Hook, context, input));
          },
          observations: () => JSON.stringify(observations),
          async wait() {
            await globalThis.__tembraCompiledLoaderTest.wait();
            return read();
          },
        },
      };
    });
    declaration.serve(
      Contributor,
      (_input, self) => {
        registerHook(self, Hook, "add", "run");
        return {
          state: {},
          methods: { run: hookHandler(Hook, (_context, args) => args.input + step) },
        };
      },
      { lifetime: "host" },
    );
  },
});

// This is a valid ordinary export in the existing ESM artifact profile.
export { facet as __esModule };
export default facet;
