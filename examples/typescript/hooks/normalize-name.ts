import { Type, defineFacet, defineHook, hookHandler, local, registerHook } from "@tembra/sdk";
import type { FacetScope, TembraHost } from "@tembra/sdk";

export const NormalizeName = defineHook("example.normalize-name", {
  version: 1,
  strategy: "waterfall",
  input: Type.String(),
});
const Normalizer = local("example.name-normalizer", {
  version: 1,
  input: Type.Object({}),
  members: { normalize: NormalizeName.method },
});

/** Portable hook authoring and execution; the Rust manager owns composition and lifecycle. */
export async function normalizeName(host: TembraHost, input: string): Promise<string> {
  host.registerHook(NormalizeName);
  let scope: FacetScope | undefined;
  await host.install(
    defineFacet({
      name: "name-normalizer",
      uses: [],
      setup(current, declaration) {
        scope = current;
        declaration.serve(
          Normalizer,
          (_input, self) => {
            registerHook(self, NormalizeName, "trim", "normalize");
            return {
              state: {},
              methods: {
                normalize: hookHandler(NormalizeName, (_context, args) => args.input.trim()),
              },
            };
          },
          { lifetime: "host" },
        );
      },
    }),
  );
  const admitted = scope;
  if (admitted === undefined) {
    throw new Error("facet setup did not issue a scope");
  }
  return await admitted.root(
    async (context) => await admitted.runHook(NormalizeName, context, input),
  );
}
