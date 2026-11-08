export { build } from "./build.ts";
export { prepareSource, type PreparedSource, type PrepareOptions } from "./prepare.ts";
export { witTypes, type WitTypes } from "./wit.ts";
export {
  Platform,
  defineConfig,
  type Artifact,
  type BuildConfig,
  type BuildTarget,
} from "./types.ts";
export { prepareBrowserEngine } from "./engine.ts";
export { prepareComponent } from "./component.ts";
export type { PreparedComponent } from "../interpreter/types.ts";
