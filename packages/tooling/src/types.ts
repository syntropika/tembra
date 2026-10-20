import type { EsmArtifactV1, ReloadReport } from "@tembra/sdk";

/** Executor environment: server means embedded V8, whose globals differ from Node.js. */
export type Platform = "server" | "browser";
/** One closed ESM bundle, checked against the selected executor's supported module profile. */
export type BuildTarget = {
  /** Authored entry file, resolved against the configured root. */
  readonly entry: string;
  /** Selects package conditions and platform diagnostics; neither target produces a Node bundle. */
  readonly platform: Platform;
  /** Loader artifact identity; defaults to this target's name in builds. */
  readonly identity?: string;
  /** Facet export selected for installation; defaults to default. */
  readonly export?: string;
  /** Minify the final source before hashing and validation; defaults to false. */
  readonly minify?: boolean;
  /** Publish a separate facet.js.map alongside the bundle; defaults to false. */
  readonly sourcemap?: boolean;
  /** Positive UTF-8 byte limit; defaults to 1,048,576. Larger settings cannot raise that loader limit. */
  readonly maxBytes?: number;
  /** Compile-time replacements: values are Bun JavaScript expressions, not raw text values. */
  readonly define?: Readonly<Record<string, string>>;
  /** Additional package export conditions; browser also selects browser, both select import/default. */
  readonly conditions?: readonly string[];
};
/** Canonical descriptor input. Paths are rooted at ToolingConfig.root; commands are explicit argv. */
export type ContractSource =
  | {
      /** Read a descriptor JSON file; no authoring source is parsed for contract inference. */
      readonly descriptor: string;
    }
  | {
      /** Trusted local TypeScript/JavaScript module evaluated with Bun authority. */
      readonly module: string;
      /** Named module export containing a contract token with a descriptor field. */
      readonly export: string;
    }
  | {
      /** Executable followed by arguments, without a shell; stdout must contain descriptor JSON. */
      readonly command: readonly string[];
      /** Command inputs watched for rebuilding; declare them explicitly, including compiled inputs. */
      readonly watch: readonly string[];
    };
/** Generate any requested outputs from one checked descriptor; at least one output is required. */
export type BindingConfig = {
  readonly source: ContractSource;
  /** Canonical descriptor JSON destination, distinct from authored sources. */
  readonly descriptor?: string;
  /** Generated TypeScript consumer destination. Relative output paths use the project root. */
  readonly typescript?: string;
  /** Generated Rust consumer destination. Uses tembra-codegen unless rustCommand overrides it. */
  readonly rust?: string;
  /** TypeScript export name; when absent, derived from the descriptor's contract name. */
  readonly name?: string;
  /** Explicit TypeScript type expressions for local nominal capabilities such as url@1. */
  readonly nativeTypes?: Readonly<Record<string, string>>;
  /** Rust generator argv, run in root with descriptor JSON on stdin; defaults to [tembra-codegen]. */
  readonly rustCommand?: readonly string[];
};
/** Bun-only build configuration. The runtime receives its resulting artifacts, never these tools. */
export type ToolingConfig = {
  /** Base directory: relative to the configuration file, or cwd for direct API calls; defaults to . */
  readonly root?: string;
  /** Named targets; names contain only ASCII letters, digits, underscores or hyphens. */
  readonly builds: Readonly<Record<string, BuildTarget>>;
  /** Artifact directory, relative to root; defaults to dist/tembra. */
  readonly outDir?: string;
  /** TypeScript project configuration, relative to root; defaults to tsconfig.json. */
  readonly tsconfig?: string;
  /** Descriptor generation runs before target typechecking; absent or empty performs no generation. */
  readonly codegen?: readonly BindingConfig[];
  readonly dev?: {
    /** File-content polling interval in milliseconds; positive integer, defaults to 100. */
    readonly pollInterval?: number;
    /** Quiet interval in milliseconds before rebuilding; nonnegative integer, defaults to 50. */
    readonly debounce?: number;
    /** Trusted local module exporting createHosts(config), returning development capabilities. */
    readonly adapter?: string;
  };
};
/** Validated configuration with absolute paths and copied collections; readonly views are not frozen. */
export type ResolvedConfig = {
  readonly root: string;
  readonly builds: Readonly<Record<string, BuildTarget>>;
  readonly outDir: string;
  readonly tsconfig: string;
  readonly codegen: readonly BindingConfig[];
  readonly dev: NonNullable<ToolingConfig["dev"]>;
  /** Source configuration path retained by loadConfig so development can load a fresh module graph. */
  readonly configFile?: string;
  /** Transitive local configuration inputs watched for fresh loading. */
  readonly configDependencies?: readonly string[];
};
/** Generated destinations and descriptor inputs, including outputs whose contents did not change. */
export type GeneratedBindings = {
  /** Absolute output paths. Existing identical content is retained without rewriting it. */
  readonly files: readonly string[];
  /** Absolute descriptor/module/command dependency paths used by development rebuild selection. */
  readonly inputs: readonly string[];
};
/** A checked artifact and its published files. Publication does not install it in any host. */
export type BuiltArtifact = {
  /** Configuration target name, used for incremental dependency tracking. */
  readonly target: string;
  readonly platform: Platform;
  /** Frozen loader envelope and export-name snapshot, derived from the final bundled source. */
  readonly artifact: EsmArtifactV1;
  readonly selectedExport: string;
  /** Absolute final bundle path inside the content-addressed publication directory. */
  readonly sourcePath: string;
  /** Absolute serialized loader envelope path, suitable for passing to an embedding. */
  readonly artifactPath: string;
  /** Absolute external source map path, present only when sourcemap was requested. */
  readonly sourcemapPath?: string;
  /** Final source size in UTF-8 bytes, excluding the map and serialized envelope. */
  readonly bytes: number;
  /** Absolute source, package and configuration inputs that can affect this target. */
  readonly inputs: readonly string[];
};
/** Successful publication result. Metadata collections are readonly views, not runtime-frozen owners. */
export type BuildResult = {
  /** Selected targets in requested order, or name order when no selection was supplied. */
  readonly artifacts: readonly BuiltArtifact[];
  readonly generated: GeneratedBindings;
  /** Sorted, deduplicated dependency paths across generation and selected targets. */
  readonly inputs: readonly string[];
};
/** Per-call selection and cooperative cancellation, captured when build is admitted. */
export type BuildOptions = {
  /** Target names; absent selects every target, an empty list runs only configured generation. */
  readonly targets?: readonly string[];
  /** Abort requests cancellation between stages; already generated or published files remain. */
  readonly signal?: AbortSignal;
};
/** Applied acknowledges settled core work; pending leaves the host unchanged without an admission. */
export type ReloadOutcome =
  | {
      readonly status: "applied";
      /** Existing core reload report when available; initial installation may have no report. */
      readonly report?: ReloadReport;
    }
  | {
      readonly status: "pending";
      /** Diagnostic explaining why no connected host admitted this delivery. */
      readonly message: string;
    };
/**
 * Possession of this embedding-issued adapter is the authority to request an update.
 * Applied acknowledges settled core work; pending means no delivery was admitted. Rejection can
 * preserve a core refusal or report a transport failure whose remote outcome remains unknown.
 */
export type DevelopmentHost = {
  /** Abort requests cancellation; settlement must join this adapter's locally owned delivery work. */
  apply: (artifacts: readonly BuiltArtifact[], signal: AbortSignal) => Promise<ReloadOutcome>;
  /** Stop admissions and join locally owned work; remote work may already have committed. */
  close?: () => Promise<void>;
};
/** One platform's acknowledged delivery result; failure is sent to onError instead. */
export type ReloadEvent = {
  readonly platform: Platform;
  readonly targets: readonly string[];
  readonly outcome: ReloadOutcome;
};
/** Development capabilities and observers, captured on admission rather than rotated in place. */
export type DevOptions = {
  /** Explicit platform capabilities. An absent host produces pending notifications after builds. */
  readonly hosts?: Partial<Readonly<Record<Platform, DevelopmentHost>>>;
  /** Requests session close and cancellation of admitted work; close still joins owned work. */
  readonly signal?: AbortSignal;
  /** Synchronous notification; returned Promises are not owned or joined by the session. */
  readonly onBuild?: (result: BuildResult) => void;
  /** Synchronous notification; returned Promises are not owned or joined by the session. */
  readonly onReload?: (event: ReloadEvent) => void;
  /** Synchronous notification; returned Promises are not owned or joined by the session. */
  readonly onError?: (error: Error) => void;
};
/** Owns watching, serialized builds and locally admitted delivery until close completes. */
export type DevSession = {
  /**
   * Resolves when the first build and available deliveries settle, including reported failure.
   * Observe onReload/onError for success; readiness alone does not establish an installed artifact.
   */
  readonly ready: Promise<void>;
  /**
   * Queue changed paths relative to root, or all targets when omitted. Build and delivery failures
   * use onError; this promise joins the queued cycle. Calls after close reject with
   * ToolingError phase cancelled and reason session_closed.
   */
  rebuild: (paths?: readonly string[]) => Promise<void>;
  /**
   * Stop admissions and join owned builds/deliveries and adapter cleanup. Cleanup failure rejects;
   * repeated calls share the same completed or failed cleanup. Remote effects are not rolled back.
   */
  close: () => Promise<void>;
};
