// Generated from crates/bridge/src/{commands,protocol,wire}. Do not edit.
// Regenerate with: TEMBRA_BLESS=1 cargo test -p tembra-bridge --test typescript
import type { Code, Lifetime, Status } from "./identifiers.ts";

type ValueOf<T> = T[keyof T];

export const KernelOp = Object.freeze({
  ContextDerive: "context-derive",
  ContextValue: "context-value",
  ContextStats: "context-stats",
  Allocate: "allocate",
  ScopeRegister: "scope-register",
  ScopeExists: "scope-exists",
  ScopesForOwner: "scopes-for-owner",
  ScopesForOrb: "scopes-for-orb",
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
  GateOrbs: "gate-orbs",
  Commit: "commit",
  Abort: "abort",
  Orbs: "orbs",
  Orb: "orb",
  OrbNamed: "orb-named",
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
  EnterExecution: "enter-execution",
  CheckContext: "check-context",
  CheckRoot: "check-root",
  EndRoot: "end-root",
  EndRootBatch: "end-root-batch",
  CancelRoot: "cancel-root",
  BeginCall: "begin-call",
  AdmitCall: "admit-call",
  BeginOperation: "begin-operation",
  BeginHookObservation: "begin-hook-observation",
  FinishCall: "finish-call",
  FinishCallBatch: "finish-call-batch",
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
  ContributionOrdinal: "contribution-ordinal",
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

export const NativeOp = Object.freeze({
  Activate: "activate",
  Dispose: "dispose",
  CancelEffect: "cancel-effect",
  Effect: "effect",
  Invoke: "invoke",
  Contribute: "contribute",
} as const);
export type NativeOp = ValueOf<typeof NativeOp>;

export const OwnerKind = Object.freeze({
  Host: "host",
  Orb: "orb",
  Object: "object",
} as const);
export type OwnerKind = ValueOf<typeof OwnerKind>;

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
  RetireOrb: "retire-orb",
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

export interface Features {
  hooks: boolean;
  subscriptions: boolean;
  views: boolean;
  telemetry: boolean;
}

export interface ContextStats {
  live: number;
  peak: number;
  keys: number;
}

export interface ContextEntryText {
  kind: "text";
  value: string;
}

export interface ContextEntryInteger {
  kind: "integer";
  value: number;
}

export interface ContextEntryBoolean {
  kind: "boolean";
  value: boolean;
}

export interface ContextEntryOpaque {
  kind: "opaque";
  source: number;
}

export type ContextEntry =
  | ContextEntryText
  | ContextEntryInteger
  | ContextEntryBoolean
  | ContextEntryOpaque;

export interface KernelError {
  code: Code;
  reason: string;
  message: string;
}

export interface ServiceInfo {
  name: string;
  methods: string[];
  signature: string | null;
  lifetime: Lifetime;
  linger: number;
}

export interface RequirementInfo {
  orb: string;
  apiVersion: number;
  services: Record<string, string[]>;
  signatures: Record<string, string>;
  hooks: string[];
}

export interface KernelOrb {
  id: number;
  owner: number;
  codeVersion: number;
  name: string;
  apiVersion: number;
  services: ServiceInfo[];
  hooks: string[];
  requires: RequirementInfo[];
  isolated: boolean;
}

export interface Reload {
  id: number;
  codeVersion: number;
  orbs: KernelOrb[];
  objects: number[];
  providers: Record<string, number>;
}

export interface AddressInfo {
  service: string;
  key: string[];
  principal: string;
}

export interface Acquisition {
  reference: number;
  object: number;
  orb: number;
  codeVersion: number;
  activate: boolean;
}

export interface Invocation {
  id: number;
  root: number;
  object: number;
  orb: number;
  codeVersion: number;
  method: string;
}

export interface HookObservation {
  invocation: Invocation;
  plan: number[];
}

export interface Snapshot {
  value: number;
  revision: string;
  version: number;
}

export interface ResourceClaim {
  id: number;
  value: number | null;
}

export interface Diagnostics {
  objects: number;
  references: number;
  roots: number;
  calls: number;
  resources: number;
  tasks: number;
  orbs: number;
  closing: boolean;
  codeVersion: number;
}

export interface Replacement {
  previous: number | null;
  next: number | null;
}

export interface EffectStop {
  type: "stop";
  scope: number;
  keepListeners: boolean;
}

export interface EffectOpen {
  type: "open";
  resource: number;
}

export interface EffectClose {
  type: "close";
  resource: number;
}

export interface EffectRestore {
  type: "restore";
  previous: number;
  resource: number;
}

export interface EffectPrepare {
  type: "prepare";
  scope: number;
  previous: number | null;
}

export interface EffectValidate {
  type: "validate";
  scope: number;
  value: number;
}

export interface EffectInitialize {
  type: "initialize";
  scope: number;
}

export interface EffectMigrate {
  type: "migrate";
  scope: number;
  value: number | null;
  version: number;
}

export interface EffectAdopt {
  type: "adopt";
  replacements: Replacement[];
}

export interface EffectStart {
  type: "start";
  scope: number;
}

export interface EffectDrop {
  type: "drop";
  scope: number;
}

export type Effect =
  | EffectStop
  | EffectOpen
  | EffectClose
  | EffectRestore
  | EffectPrepare
  | EffectValidate
  | EffectInitialize
  | EffectMigrate
  | EffectAdopt
  | EffectStart
  | EffectDrop;

export interface ActionExecute {
  type: "execute";
  id: number;
  effects: Effect[];
}

export interface ActionCancelEffects {
  type: "cancel-effects";
  id: number;
  effects: Effect[];
}

export interface ActionProgramPrepared {
  type: "program-prepared";
  id: number;
}

export interface ActionProgramComplete {
  type: "program-complete";
  id: number;
  error: KernelError | null;
}

export interface ActionDispose {
  type: "dispose";
  id: number;
}

export interface ActionCloseResource {
  type: "close-resource";
  id: number;
}

export interface ActionDropValue {
  type: "drop-value";
  id: number;
}

export interface ActionCancelRoot {
  type: "cancel-root";
  id: number;
}

export interface ActionCancelTask {
  type: "cancel-task";
  id: number;
}

export interface ActionRetireOrb {
  type: "retire-orb";
  id: number;
}

export interface ActionState {
  type: "state";
  id: number;
  revision: string;
}

export interface ActionSubscriptionFailed {
  type: "subscription-failed";
  id: number;
}

export interface ActionDetachSubscription {
  type: "detach-subscription";
  id: number;
}

export interface ActionDeliver {
  type: "deliver";
  id: number;
  payload: number;
  operation: number;
}

export type Action =
  | ActionExecute
  | ActionCancelEffects
  | ActionProgramPrepared
  | ActionProgramComplete
  | ActionDispose
  | ActionCloseResource
  | ActionDropValue
  | ActionCancelRoot
  | ActionCancelTask
  | ActionRetireOrb
  | ActionState
  | ActionSubscriptionFailed
  | ActionDetachSubscription
  | ActionDeliver;

export interface StepExecute {
  type: "execute";
  effects: Effect[];
}

export interface StepWait {
  type: "wait";
  deadline: string | null;
}

export interface StepPrepared {
  type: "prepared";
}

export interface StepComplete {
  type: "complete";
}

export interface StepFailed {
  type: "failed";
  error: KernelError;
}

export type Step = StepExecute | StepWait | StepPrepared | StepComplete | StepFailed;

export interface CallSettlement {
  actions: Action[];
  deadline: number | null;
}

export type Owner = { kind: "host" } | { kind: "orb" | "object"; id: number };

export interface ContextKeyInput {
  name: string;
  version: number;
  kind: "text" | "integer" | "boolean" | "opaque";
}

export interface ServiceDeclaration {
  name: string;
  methods: readonly string[];
  signature?: string | null | undefined;
  lifetime?: Lifetime | null | undefined;
  linger?: number | null | undefined;
}

export interface RequirementDeclaration {
  orb: string;
  apiVersion: number;
  services?: Readonly<Record<string, readonly string[]>> | null | undefined;
  signatures?: Readonly<Record<string, string>> | null | undefined;
  hooks?: readonly string[] | null | undefined;
}

export interface OrbDeclaration {
  name: string;
  apiVersion: number;
  services: readonly ServiceDeclaration[];
  requires?: readonly RequirementDeclaration[] | null | undefined;
  hooks?: readonly string[] | null | undefined;
  isolated?: boolean | null | undefined;
}

export interface MigrationInput {
  object: number;
  revision: number | string;
  value: number;
  version: number;
}

export interface ReplacementInput {
  previous?: number | null | undefined;
  next?: number | null | undefined;
}

export interface ErrorInput {
  code: Code;
  reason: string;
  message: string;
}

export interface CompletionInput {
  value?: number | null | undefined;
  error?: ErrorInput | null | undefined;
}

/** Commands without input fields. */
export type EmptyInput = Record<string, never>;

/** Derive a child context frame carrying a typed scalar or an owner-issued reference. */
export interface ContextDeriveInput {
  root: number;
  context?: number | null | undefined;
  call?: number | null | undefined;
  key: ContextKeyInput;
  value?: string | number | boolean | null | undefined;
  source?: number | null | undefined;
}

/** Read the nearest binding of a typed context key. */
export interface ContextValueInput {
  root: number;
  context?: number | null | undefined;
  call?: number | null | undefined;
  key: ContextKeyInput;
}

/** Describe a declared scope to the lifecycle engine. */
export interface ScopeRegisterInput {
  scope: number;
  owner: Owner;
  orb: number;
  parents?: readonly number[] | null | undefined;
  dependencies?: readonly number[] | null | undefined;
  resources?: readonly number[] | null | undefined;
  stateVersion?: number | null | undefined;
  migrate?: boolean | null | undefined;
}

/** Report whether a scope is registered. */
export interface ScopeExistsInput {
  scope: number;
}

/** List scopes owned by the host, an orb or an object. */
export interface ScopesForOwnerInput {
  owner: Owner;
}

/** List scopes of one orb version. */
export interface ScopesForOrbInput {
  orb: number;
}

/** Attach a resource to a registered scope. */
export interface ScopeAddResourceInput {
  scope: number;
  resource: number;
}

/** Start the activation program of a registered scope. */
export interface ScopeActivateInput {
  scope: number;
}

/** Start a cleanup program for scopes. */
export interface ScopesCleanupInput {
  scopes: readonly number[];
  object?: number | null | undefined;
}

/** Reload every registered candidate scope of a prepared ticket. */
export interface ScopesReloadRegisteredInput {
  ticket: number;
  deadline: number | string;
  now?: number | string | null | undefined;
}

/** Reload explicit previous/next scope pairs of a prepared ticket. */
export interface ScopesReloadInput {
  ticket: number;
  deadline: number | string;
  now?: number | string | null | undefined;
  replacements: readonly ReplacementInput[];
}

/** Start a prepared lifecycle program. */
export interface ProgramRunInput {
  program: number;
}

/** Read a program's next step without executing callbacks. */
export interface ProgramNextInput {
  program: number;
}

/** Acknowledge the physical settlement of every effect in the current step. */
export interface ProgramCompleteStepInput {
  program: number;
  results: readonly CompletionInput[];
}

/** Commit a prepared reload program. */
export interface ProgramCommitInput {
  program: number;
}

/** Abort a reload program. */
export interface ProgramAbortInput {
  program: number;
}

/** Forget a settled program. */
export interface ProgramForgetInput {
  program: number;
}

/** Prepare orb contracts as a reload candidate. */
export interface PrepareInput {
  orbs: readonly OrbDeclaration[];
  remove?: readonly string[] | null | undefined;
}

/** Gate candidate objects of a prepared ticket. */
export interface GateInput {
  ticket: number;
  objects: readonly number[];
}

/** Gate candidate orb owners of a prepared ticket. */
export interface GateOrbsInput {
  ticket: number;
  owners: readonly number[];
}

/** Commit a prepared ticket and its state migrations. */
export interface CommitInput {
  ticket: number;
  migrations?: readonly MigrationInput[] | null | undefined;
}

/** Abort a prepared ticket. */
export interface AbortInput {
  ticket: number;
}

/** Describe one installed orb version. */
export interface OrbInput {
  id: number;
}

/** Find an installed orb, or a candidate of a prepared ticket, by name. */
export interface OrbNamedInput {
  name: string;
  ticket?: number | null | undefined;
}

/** Read a managed object's address. */
export interface AddressInput {
  object: number;
}

/** Claim a reference to a managed object. */
export interface AcquireInput {
  service: string;
  key: readonly string[];
  principal: string;
  apiVersion?: number | null | undefined;
  methods?: readonly string[] | null | undefined;
}

/** Mark an activated object live. */
export interface ReadyInput {
  object: number;
}

/** Record that an object depends on a reference. */
export interface DependencyInput {
  object: number;
  reference: number;
}

/** Detach an object's dependency. */
export interface UndependInput {
  object: number;
  reference: number;
}

/** Mark an object's activation as failed. */
export interface FailInput {
  object: number;
}

/** Read a reference's status. */
export interface StatusInput {
  reference: number;
}

/** Release a reference claim. */
export interface ReleaseInput {
  reference: number;
}

/** Forget a released reference. */
export interface ForgetInput {
  reference: number;
}

/** Begin a root for a principal. */
export interface RootInput {
  principal: string;
}

/** Begin a cleanup root for a principal. */
export interface CleanupRootInput {
  principal: string;
}

/** Read a root's principal. */
export interface RootPrincipalInput {
  root: number;
}

/** Enter the invocation's isolated heap under its retained private execution ancestry. */
export interface EnterExecutionInput {
  call: number;
  context?: number | null | undefined;
}

/** Recheck a root and, when supplied, that an invocation belongs to it. */
export interface CheckContextInput {
  root: number;
  call?: number | null | undefined;
}

/** Reject a cancelled or ended root. */
export interface CheckRootInput {
  root: number;
}

/** End a root. */
export interface EndRootInput {
  root: number;
}

/** End logical root admission and return all resulting cleanup metadata in one exchange. */
export interface EndRootBatchInput {
  root: number;
}

/** Cancel a root. */
export interface CancelRootInput {
  root: number;
}

/** Admit a service call through a reference. */
export interface BeginCallInput {
  reference: number;
  root: number;
  method: string;
  context?: number | null | undefined;
}

/** Recheck the caller's context, then admit a service call, in one bounded exchange. */
export interface AdmitCallInput {
  reference: number;
  root: number;
  parent?: number | null | undefined;
  method: string;
  context?: number | null | undefined;
}

/** Admit an owner-bound callback operation. */
export interface BeginOperationInput {
  orb: number;
  root: number;
  operation: number;
  object?: number | null | undefined;
  context?: number | null | undefined;
}

/** Admit the single owner-bound observation of a terminal hook. */
export interface BeginHookObservationInput {
  source: number;
  orb: number;
  operation: number;
  object?: number | null | undefined;
  hook: string;
}

/** Finish a physical call. */
export interface FinishCallInput {
  call: number;
}

/** Tick, finish a physical call, drain its actions and read the next deadline in one exchange. */
export interface FinishCallBatchInput {
  call: number;
  now: number | string;
}

/** Describe an admitted call. */
export interface InvocationInput {
  call: number;
}

/** Initialize an object's state. */
export interface StateInitInput {
  object: number;
  value: number;
  version: number;
}

/** Read an object's state metadata. */
export interface SnapshotInput {
  object: number;
}

/** Adopt a new state value when the expected revision still matches. */
export interface StateAdoptInput {
  call: number;
  object: number;
  revision: number | string;
  value: number;
}

/** Begin a background task. */
export interface TaskInput {
  owner: Owner;
}

/** Begin a root for a task. */
export interface TaskRootInput {
  task: number;
  principal: string;
}

/** Request that a task stop. */
export interface StopTaskInput {
  task: number;
}

/** Finish a task. */
export interface FinishTaskInput {
  task: number;
}

/** Claim a named resource. */
export interface ResourceInput {
  owner: Owner;
  name: string;
  version: number;
  exclusive?: boolean | null | undefined;
  dependencies?: readonly number[] | null | undefined;
}

/** Record a resource's opened value. */
export interface ResourceOpenedInput {
  resource: number;
  value: number;
}

/** Read a resource's opened value. */
export interface ResourceValueInput {
  resource: number;
}

/** Request a resource close before an optional version. */
export interface CloseResourceInput {
  resource: number;
  before?: number | null | undefined;
}

/** Record that a resource closed. */
export interface ResourceClosedInput {
  resource: number;
}

/** Record that an object was disposed. */
export interface DisposedInput {
  object: number;
}

/** Advance host-relative monotonic time. */
export interface TickInput {
  now: number | string;
}

/** Restrict the methods a reference may call. */
export interface RestrictInput {
  reference: number;
  methods: readonly string[];
}

/** Register a hook contribution. */
export interface ContributeInput {
  orb: number;
  target: string;
  hook: string;
  operation: number;
  priority?: number | null | undefined;
  ordinal: number;
}

/** Read the admitted contribution plan of a hook. */
export interface HookPlanInput {
  root: number;
  target: string;
  hook: string;
}

/** Connect a scope listener to a source. */
export interface ListenInput {
  scope: number;
  previous?: number | null | undefined;
  name: string;
  source: number;
  sizing?: number | null | undefined;
  operation: number;
  maxPending: number;
  maxBytes?: number | null | undefined;
  coalesce?: boolean | null | undefined;
}

/** Subscribe an owner to a source. */
export interface SubscribeInput {
  owner: Owner;
  source: number;
  operation: number;
  maxPending: number;
  maxBytes?: number | null | undefined;
  coalesce?: boolean | null | undefined;
}

/** Queue a delivery. */
export interface EnqueueInput {
  subscription: number;
  payload: number;
  bytes?: number | null | undefined;
}

/** Take the next queued delivery payload. */
export interface NextDeliveryInput {
  subscription: number;
}

/** Finish the current delivery. */
export interface FinishDeliveryInput {
  subscription: number;
}

/** Remove a subscription. */
export interface UnsubscribeInput {
  subscription: number;
}

/** Every kernel operation with its typed input and the result it determines. */
export interface KernelCommands {
  "context-derive": { input: ContextDeriveInput; output: number };
  "context-value": { input: ContextValueInput; output: ContextEntry | null };
  "context-stats": { input: EmptyInput; output: ContextStats };
  allocate: { input: EmptyInput; output: number };
  "scope-register": { input: ScopeRegisterInput; output: null };
  "scope-exists": { input: ScopeExistsInput; output: boolean };
  "scopes-for-owner": { input: ScopesForOwnerInput; output: number[] };
  "scopes-for-orb": { input: ScopesForOrbInput; output: number[] };
  "scope-add-resource": { input: ScopeAddResourceInput; output: null };
  "scope-activate": { input: ScopeActivateInput; output: number };
  "scopes-cleanup": { input: ScopesCleanupInput; output: number };
  "scopes-reload-registered": { input: ScopesReloadRegisteredInput; output: number };
  "scopes-reload": { input: ScopesReloadInput; output: number };
  "program-run": { input: ProgramRunInput; output: null };
  "program-next": { input: ProgramNextInput; output: Step };
  "program-complete-step": { input: ProgramCompleteStepInput; output: null };
  "program-commit": { input: ProgramCommitInput; output: null };
  "program-abort": { input: ProgramAbortInput; output: null };
  "program-forget": { input: ProgramForgetInput; output: null };
  features: { input: EmptyInput; output: Features };
  prepare: { input: PrepareInput; output: Reload };
  gate: { input: GateInput; output: boolean };
  "gate-orbs": { input: GateOrbsInput; output: boolean };
  commit: { input: CommitInput; output: number };
  abort: { input: AbortInput; output: null };
  orbs: { input: EmptyInput; output: KernelOrb[] };
  orb: { input: OrbInput; output: KernelOrb };
  "orb-named": { input: OrbNamedInput; output: KernelOrb };
  "next-deadline": { input: EmptyInput; output: number | null };
  address: { input: AddressInput; output: AddressInfo };
  acquire: { input: AcquireInput; output: Acquisition };
  ready: { input: ReadyInput; output: null };
  dependency: { input: DependencyInput; output: null };
  undepend: { input: UndependInput; output: null };
  fail: { input: FailInput; output: null };
  status: { input: StatusInput; output: Status };
  release: { input: ReleaseInput; output: null };
  forget: { input: ForgetInput; output: null };
  root: { input: RootInput; output: number };
  "cleanup-root": { input: CleanupRootInput; output: number };
  "root-principal": { input: RootPrincipalInput; output: string };
  "enter-execution": { input: EnterExecutionInput; output: number };
  "check-context": { input: CheckContextInput; output: null };
  "check-root": { input: CheckRootInput; output: null };
  "end-root": { input: EndRootInput; output: null };
  "end-root-batch": { input: EndRootBatchInput; output: CallSettlement };
  "cancel-root": { input: CancelRootInput; output: null };
  "begin-call": { input: BeginCallInput; output: Invocation };
  "admit-call": { input: AdmitCallInput; output: Invocation };
  "begin-operation": { input: BeginOperationInput; output: Invocation };
  "begin-hook-observation": { input: BeginHookObservationInput; output: HookObservation };
  "finish-call": { input: FinishCallInput; output: null };
  "finish-call-batch": { input: FinishCallBatchInput; output: CallSettlement };
  invocation: { input: InvocationInput; output: Invocation };
  "state-init": { input: StateInitInput; output: Snapshot };
  snapshot: { input: SnapshotInput; output: Snapshot };
  "state-adopt": { input: StateAdoptInput; output: Snapshot };
  task: { input: TaskInput; output: number };
  "task-root": { input: TaskRootInput; output: number };
  "stop-task": { input: StopTaskInput; output: null };
  "finish-task": { input: FinishTaskInput; output: null };
  resource: { input: ResourceInput; output: ResourceClaim };
  "resource-opened": { input: ResourceOpenedInput; output: null };
  "resource-value": { input: ResourceValueInput; output: number };
  "close-resource": { input: CloseResourceInput; output: boolean };
  "resource-closed": { input: ResourceClosedInput; output: null };
  disposed: { input: DisposedInput; output: null };
  tick: { input: TickInput; output: null };
  "next-wake": { input: EmptyInput; output: number | null };
  "next-idle": { input: EmptyInput; output: number | null };
  close: { input: EmptyInput; output: null };
  "finish-close": { input: EmptyInput; output: null };
  diagnostics: { input: EmptyInput; output: Diagnostics };
  drain: { input: EmptyInput; output: Action[] };
  restrict: { input: RestrictInput; output: null };
  "contribution-ordinal": { input: EmptyInput; output: number };
  contribute: { input: ContributeInput; output: null };
  "hook-plan": { input: HookPlanInput; output: number[] };
  listen: { input: ListenInput; output: number };
  subscribe: { input: SubscribeInput; output: number };
  enqueue: { input: EnqueueInput; output: null };
  "next-delivery": { input: NextDeliveryInput; output: number | null };
  "finish-delivery": { input: FinishDeliveryInput; output: null };
  unsubscribe: { input: UnsubscribeInput; output: null };
}

export type KernelInput<K extends KernelOp> = KernelCommands[K]["input"];
export type KernelOutput<K extends KernelOp> = KernelCommands[K]["output"];

/** The structured result of one platform exchange. */
export type KernelOutcome<T> =
  | { ok: T; error?: undefined }
  | { ok?: undefined; error: KernelError };
