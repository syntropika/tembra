export interface BuildTarget {
  entry: string;
  wit: string;
  world?: string;
  /** Compiled components receive their identity from this target's name and API version. */
  apiVersion?: number;
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
