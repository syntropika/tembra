import { Lifetime, ReloadStatus, Status } from "../bridge/identifiers.ts";
import type { Invocation, KernelOrb, KernelPort } from "../bridge/kernel.ts";
import type { Codec } from "../codecs/json.ts";
import type { Context } from "../runtime/context.ts";
import type { Readable } from "../state.ts";
import type { Declaration } from "./declaration.ts";

export type Completion<T> = T | PromiseLike<T>;

// Executable definitions are heterogeneous. These dynamic positions carry caller-owned types;
// runtime authority and ownership are established by the kernel, not TypeScript casts.
export type Handler = (environment: any, context: Context, ...arguments_: any[]) => unknown;
export interface ServiceDefinition {
  create?: (self: Declaration) => unknown;
  methods: Record<string, Handler>;
  lifetime?: Lifetime;

  /** Idle milliseconds to retain an unreferenced object and its running tasks; defaults to zero. */
  linger?: number;
  access?: (principal: string) => { methods: readonly string[] };
}

export interface Requirement {
  orb?: string;
  apiVersion: number;
  methods?: readonly string[];
  services?: Readonly<Record<string, readonly string[]>>;
  hooks?: readonly string[];
}

export interface HookDefinition {
  capabilities?: readonly string[];

  /** Execute a hook with its admitted participants. */
  run(context: Context, input: any, participants: readonly Participant[]): Completion<unknown>;
}
export type Participant = (
  context: Context,
  input: any,
  next?: (input: any) => Promise<unknown>,
) => Promise<any>;
export type ContributionHandler = (
  context: Context,
  input: any,
  next?: (input: any) => Promise<unknown>,
) => unknown;
export type Contribution = ContributionHandler | { handle: ContributionHandler; priority?: number };

export interface OrbDefinition {
  name: string;
  apiVersion: number;
  requires?: Readonly<Record<string, Requirement>>;
  create?: (self: Declaration) => unknown;
  services?: Readonly<Record<string, ServiceDefinition>>;
  hooks?: Readonly<Record<string, HookDefinition>>;
  contributes?: Readonly<Record<string, Readonly<Record<string, Contribution>>>>;
}

export interface ResourceOptions<T> {
  version?: number;
  exclusive?: boolean;
  opaque?: boolean;
  deps?: readonly unknown[];

  /** Create the resource value and observe cancellation when supported. */
  open(signal: AbortSignal): Completion<T>;

  /** Release the value after its physical users settle. */
  close?(value: T, context: Context): Completion<unknown>;
}

export interface Resource<T> {
  readonly value: T;
  readonly version: number;
}
export interface TaskScope {
  readonly signal: AbortSignal;

  /** Run one operation under this task's authority. */
  root<T>(operation: (context: Context) => Completion<T>): Promise<T>;
}
export interface EventSource<T> {
  /** Attach a listener and return its detach function. */
  subscribe(listener: (value: T) => void): () => void;
}
export interface ListenerOptions {
  maxPending?: number;
  maxBytes?: number;
  sizeOf?: (value: any) => number;
}

export interface ReferenceBase<T = unknown> {
  readonly ready: Promise<void>;
  readonly status: Status;
  readonly state: Readable<T>;

  /** Release this reference's ownership claim. */
  release(): void;

  /** Read an explicitly exposed local resource. */
  opaque<R = unknown>(name: string): R;
}
export type DynamicReference = ReferenceBase<any> & Record<string, any>;

export interface AcquisitionOptions {
  apiVersion?: number;
  methods?: readonly string[];
  principal?: string;
}
export interface ReloadOptions {
  deadline?: number;
}
export interface ReloadReport {
  codeVersion: number;
  orbs: readonly string[];
}
export interface PreparedReload {
  readonly report: ReloadReport;
  readonly status: ReloadStatus;

  /** Adopt the prepared replacement and start its scopes. */
  commit(): Promise<ReloadReport>;

  /** Discard the candidate and restore previous scopes. */
  abort(): Promise<void>;
}

export interface NativeAdapter {
  /** Revoke native reference claims issued inside a JS-owned root. */
  rootEnded?(root: number): void;
  /** Return a native service's principal-specific method policy when configured. */
  access?(orb: number, service: string, principal: string): readonly string[] | undefined;
  /** Await native callbacks while this realm keeps dispatching their replies. */
  settled?(): Promise<void>;
  /** Mirror shared kernel notifications into native callback storage. */
  notify?(action: import("../bridge/kernel.ts").Action): boolean;

  /** Check whether Rust owns a managed hook participant. */
  hasContribution?(operation: number): boolean;

  /** Execute a native hook participant while retaining its physical lifetime. */
  contribute?(operation: number, context: Context, input: unknown): Promise<unknown>;

  /** Check whether Rust owns this orb implementation. */
  hasOrb(id: number): boolean;

  /** Declare the native object's scopes before activation. */
  activate(
    object: number,
    orb: number,
    service: string,
    key: readonly string[],
    principal: string,
  ): Promise<void>;

  /** Execute an admitted call in Rust. */
  invoke(
    invocation: Invocation,
    arguments_: readonly unknown[],
    context: Context,
  ): Promise<unknown>;

  /** Release native object storage after scope cleanup. */
  dispose(object: number): Promise<void>;

  /** Execute a language callback requested by the kernel. */
  effect(effect: import("../bridge/kernel.ts").Effect): Promise<number | void>;

  /** Signals cancellation without acknowledging physical effect completion. */
  cancelEffect?(effect: import("../bridge/kernel.ts").Effect): Completion<void>;

  /** Decode a native value for the JS adapter. */
  value(id: number): unknown;

  /** Release the native value's bridge slot. */
  dropValue?(id: number): void;
}

export interface HostOptions {
  /** Explicit monotonic origin for adapters sharing an already-running kernel. */
  clockOrigin?: number;
  name?: string;
  principal?: string;
  kernel: KernelPort;
  serialization?: Codec;
  load?: (location: string | URL) => Promise<OrbDefinition>;

  /** Receives adapter and observer failures; throwing from this callback cannot interrupt dispatch. */
  onError?: (error: Error) => void;

  /** Monotonic milliseconds from any origin; each host measures time relative to its creation. */
  now?: () => number;
  native?: NativeAdapter;
  resources?: Readonly<Record<string, ResourceOptions<unknown>>>;
  telemetry?: { capacity?: number; sampleEvery?: number };
}

export interface OrbDescription extends KernelOrb {
  /** Read metadata without activating orb services. */
  describe(): Readonly<KernelOrb>;
}
