/** Closed adapter vocabulary. Strings are retained only as the wire representation. */
type ValueOf<T> = T[keyof T];

export const KernelOp = Object.freeze({
  Allocate: "allocate",
  ScopeRegister: "scope-register",
  ScopeExists: "scope-exists",
  ScopesForOwner: "scopes-for-owner",
  ScopesForFacet: "scopes-for-facet",
  ScopeAddResource: "scope-add-resource",
  ScopeActivate: "scope-activate",
  ScopesCleanup: "scopes-cleanup",
  ScopesReloadRegistered: "scopes-reload-registered",
  ScopesReload: "scopes-reload",
  ProgramRun: "program-run",
  ProgramNext: "program-next",
  ProgramCompleteStep: "program-complete-step",
  ProgramCommit: "program-commit",
  ProgramAbort: "program-abort",
  ProgramForget: "program-forget",
  Features: "features",
  Prepare: "prepare",
  Gate: "gate",
  GateFacets: "gate-facets",
  Commit: "commit",
  Abort: "abort",
  Facets: "facets",
  Facet: "facet",
  FacetNamed: "facet-named",
  NextDeadline: "next-deadline",
  Address: "address",
  Acquire: "acquire",
  Ready: "ready",
  Dependency: "dependency",
  Undepend: "undepend",
  Fail: "fail",
  Status: "status",
  Release: "release",
  Forget: "forget",
  Root: "root",
  CleanupRoot: "cleanup-root",
  RootPrincipal: "root-principal",
  CheckContext: "check-context",
  CheckRoot: "check-root",
  EndRoot: "end-root",
  CancelRoot: "cancel-root",
  BeginCall: "begin-call",
  BeginOperation: "begin-operation",
  FinishCall: "finish-call",
  Invocation: "invocation",
  StateInit: "state-init",
  Snapshot: "snapshot",
  StateAdopt: "state-adopt",
  Task: "task",
  TaskRoot: "task-root",
  StopTask: "stop-task",
  FinishTask: "finish-task",
  Resource: "resource",
  ResourceOpened: "resource-opened",
  ResourceValue: "resource-value",
  CloseResource: "close-resource",
  ResourceClosed: "resource-closed",
  Disposed: "disposed",
  Tick: "tick",
  NextWake: "next-wake",
  NextIdle: "next-idle",
  Close: "close",
  FinishClose: "finish-close",
  Diagnostics: "diagnostics",
  Drain: "drain",
  Restrict: "restrict",
  Contribute: "contribute",
  HookPlan: "hook-plan",
  Listen: "listen",
  Subscribe: "subscribe",
  Enqueue: "enqueue",
  NextDelivery: "next-delivery",
  FinishDelivery: "finish-delivery",
  Unsubscribe: "unsubscribe",
} as const);
export type KernelOp = ValueOf<typeof KernelOp>;

export const ActionType = Object.freeze({
  Execute: "execute",
  CancelEffects: "cancel-effects",
  ProgramPrepared: "program-prepared",
  ProgramComplete: "program-complete",
  Dispose: "dispose",
  CloseResource: "close-resource",
  DropValue: "drop-value",
  CancelRoot: "cancel-root",
  CancelTask: "cancel-task",
  RetireFacet: "retire-facet",
  State: "state",
  SubscriptionFailed: "subscription-failed",
  DetachSubscription: "detach-subscription",
  Deliver: "deliver",
} as const);
export type ActionType = ValueOf<typeof ActionType>;

export const EffectType = Object.freeze({
  Stop: "stop",
  Open: "open",
  Close: "close",
  Restore: "restore",
  Prepare: "prepare",
  Validate: "validate",
  Initialize: "initialize",
  Migrate: "migrate",
  Adopt: "adopt",
  Start: "start",
  Drop: "drop",
} as const);
export type EffectType = ValueOf<typeof EffectType>;

export const StepType = Object.freeze({
  Execute: "execute",
  Wait: "wait",
  Prepared: "prepared",
  Complete: "complete",
  Failed: "failed",
} as const);
export type StepType = ValueOf<typeof StepType>;

export const NativeOp = Object.freeze({
  Activate: "activate",
  Dispose: "dispose",
  CancelEffect: "cancel-effect",
  Effect: "effect",
  Invoke: "invoke",
} as const);
export type NativeOp = ValueOf<typeof NativeOp>;

export const RpcOp = Object.freeze({
  Describe: "describe",
  Get: "get",
  Release: "release",
  Call: "call",
  Cancel: "cancel",
} as const);
export type RpcOp = ValueOf<typeof RpcOp>;

export const RpcEvent = Object.freeze({
  State: "state",
} as const);
export type RpcEvent = ValueOf<typeof RpcEvent>;

export const OwnerKind = Object.freeze({
  Host: "host",
  Facet: "facet",
  Object: "object",
} as const);
export type OwnerKind = ValueOf<typeof OwnerKind>;

export const EnvironmentPhase = Object.freeze({
  Declaring: "declaring",
  Prepared: "prepared",
  Live: "live",
  Closed: "closed",
} as const);
export type EnvironmentPhase = ValueOf<typeof EnvironmentPhase>;

export const CoordinationDecision = Object.freeze({
  Preparing: "preparing",
  Commit: "commit",
  Abort: "abort",
} as const);
export type CoordinationDecision = ValueOf<typeof CoordinationDecision>;

export const Outcome = Object.freeze({
  Ok: "ok",
  Error: "error",
} as const);
export type Outcome = ValueOf<typeof Outcome>;

export const CodecTag = Object.freeze({
  Bigint: "bigint",
  Bytes: "bytes",
  Object: "object",
} as const);
export type CodecTag = ValueOf<typeof CodecTag>;

export const CodecId = Object.freeze({
  Json: "tembra-json/1",
  MessagePack: "tembra-msgpack/1",
} as const);
export type CodecId = ValueOf<typeof CodecId>;
