export const Platform = Object.freeze({
  Browser: "browser",
  Worker: "worker",
  Node: "node",
  Embedded: "embedded",
} as const);
export type Platform = (typeof Platform)[keyof typeof Platform];
export interface BuildTarget {
  entry: string;
  platform: Platform;
  minify?: boolean;
  typecheck?: boolean;
  nativeExternals?: readonly string[];
  define?: Readonly<Record<string, string>>;
}
export interface BuildConfig {
  root?: string;
  outDir?: string;
  builds: Readonly<Record<string, BuildTarget>>;
}
export interface Artifact {
  name: string;
  platform: Platform;
  path: string;
  sha256: string;
  bytes: number;
}

/** Preserve build target types in a tooling configuration. */
export function defineConfig<const C extends BuildConfig>(config: C): C {
  return config;
}
