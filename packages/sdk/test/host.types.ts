import type { BrowserHost } from "../src/browser/index.js";
import type { DynamicFacet, EsmArtifactV1, FacetDefinition, TembraHost } from "../src/index.js";
import type { NodeHost } from "../src/node/index.js";

async function hostInputs(host: TembraHost, facet: FacetDefinition, artifact: EsmArtifactV1) {
  await host.install(facet);
  await host.load("./facet.ts");
  await host.load(new URL("https://example.com/facet.js"), "named");
  await host.load(artifact);
  const candidates: readonly DynamicFacet[] = [
    facet,
    "./facet.ts",
    new URL("https://example.com/facet.js"),
    { module: "./facet.ts", selected: "named" },
    artifact,
    { artifact, selected: "named" },
  ];
  await host.reload(candidates);
  // @ts-expect-error: evaluated callbacks use install, not load
  await host.load(facet);
  // @ts-expect-error: module references are not evaluated callback facets
  await host.install("./facet.ts");
  // @ts-expect-error: a selected export is a string
  await host.reload([{ module: "./facet.ts", selected: 1 }]);
}
function platformCompatibility(node: NodeHost, browser: BrowserHost): readonly TembraHost[] {
  return [node, browser];
}
void [hostInputs, platformCompatibility];
