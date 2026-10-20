/** Cumulative admission for the Node environment; managed retirement never refunds these costs. */
type AdmissionDiagnostics = {
  readonly policy: "javascript-lifetime-budget";
  readonly physicalUnloadSupported: false;
  readonly source: {
    readonly profile: "node-source-v1";
    readonly physicalUnloadSupported: false;
    readonly evaluations: number;
    readonly sourceBytes: number;
    readonly modules: number;
    readonly evaluationFailures: number;
    readonly maxEvaluations: number;
    readonly maxSourceBytes: number;
    readonly maxModules: number;
    readonly maxModuleBytes: number;
    readonly warning: boolean;
    readonly requiresProcessRotation: boolean;
  };
  readonly evaluations: number;
  readonly sourceBytes: number;
  readonly maxEvaluations: number;
  readonly maxSourceBytes: number;
  readonly maxArtifactBytes: number;
  readonly remainingEvaluations: number;
  readonly remainingSourceBytes: number;
  readonly evaluationFailures: number;
  readonly warning: boolean;
  readonly requiresProcessRotation: boolean;
  readonly inFlight: null | {
    readonly stage: "validating" | "reserved" | "evaluating" | "committing";
    readonly elapsedMs: number;
    /** Unsubmitted entries in both artifact and source queues; the active evaluation is already charged. */
    readonly reservedEvaluations: number;
    /** UTF-8 input bytes of those queued entries; excludes already charged transforms and dependencies. */
    readonly reservedSourceBytes: number;
  };
};

export type NodeLoaderDiagnostics = Omit<AdmissionDiagnostics, "source"> & {
  readonly source: AdmissionDiagnostics["source"] & {
    readonly evaluator: string;
    readonly compilerRealm: "isolated";
  };
  readonly executionProfile: "tembra-esm-v1";
  readonly evaluator: string;
};

/** A binding owner observation, independent of physical module collection. */
export type NodeGenerationDiagnostics = {
  readonly id: string;
  readonly kind: "installed" | "esm" | "source";
  readonly sourceHash: string | null;
  readonly ageMs: number;
  readonly state: "prepared" | "managed" | "retiring";
  readonly runOwners: number;
  readonly activeRuns: number;
  readonly callbacks: number;
  readonly values: number;
  readonly physicalCallbacks: number;
};

export type NativeHostDiagnostics = {
  readonly codeVersion: number;
  readonly loader: AdmissionDiagnostics;
  readonly generations: readonly NodeGenerationDiagnostics[];
  readonly releasedGenerations: readonly {
    readonly id: string;
    readonly kind: "installed" | "esm" | "source";
    readonly sourceHash: string | null;
    readonly state: "managedReleased";
  }[];
  readonly jobs: number;
  readonly callbackJobs: number;
  readonly handles: number;
  readonly handlesByKind: Readonly<Record<string, number>>;
  readonly errors: readonly unknown[];
};

/** Detached diagnostics remain available during close and as a final snapshot after close. */
export type NodeHostDiagnostics = Omit<NativeHostDiagnostics, "loader"> & {
  readonly loader: NodeLoaderDiagnostics;
  readonly bridge: {
    readonly callbacks: number;
    readonly values: number;
    readonly wrappers: number;
    readonly jobs: number;
  };
  /** Runtime and process observations, not allocation accounting or memory limits for an individual facet. */
  readonly memory: {
    readonly rss: number;
    readonly heapTotal: number;
    readonly heapUsed: number;
    readonly external: number;
    readonly arrayBuffers: number;
    readonly heapSizeLimit: number;
  };
};
