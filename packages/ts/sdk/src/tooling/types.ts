/** WIT imports the bundle may keep external, by who may import them. */
export interface WitImports {
  /** Imports author modules may use directly: the world's standard `wasi:*` interfaces. */
  readonly direct: ReadonlySet<string>;
  /**
   * Imports only the generated entry may use: the private host, Orb dependencies (reached through
   * `Declaration.get`/`Context.get`) and hook self-imports.
   */
  readonly entry: ReadonlySet<string>;
}

export interface BuildTarget {
  entry: string;
  wit: string;
  world?: string;
  /** Compiled components receive their identity from this target's name and API version. */
  apiVersion?: number;
  /** Compiled callback interfaces contributing to their selected Orb's hook, with execution priority. */
  contributes?: Readonly<Record<string, number>>;
  minify?: boolean;
  define?: Readonly<Record<string, string>>;
}
export interface BuildConfig {
  root?: string;
  outDir?: string;
  builds: Readonly<Record<string, BuildTarget>>;
}
export interface Artifact {
  /** Build target key; the source Orb's installation identity belongs to its literal declaration. */
  name: string;
  path: string;
  sha256: string;
  bytes: number;
}

/** Preserve literal build configuration types. */
export function defineConfig<const C extends BuildConfig>(config: C): C {
  return config;
}
