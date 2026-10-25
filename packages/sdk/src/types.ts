import { Lifetime, ReloadStatus, Status } from "./identifiers.ts";
import type { Context } from "./runtime/context.ts";
import type { Declaration } from "./runtime/declaration.ts";
import type { Codec } from "./codec.ts";
import type { KernelPort, KernelFacet, Invocation } from "./kernel.ts";
import type { Readable } from "./state.ts";

export type Completion<T> = T | PromiseLike<T>;

// Executable definitions are heterogeneous. These dynamic positions carry caller-owned types;
// runtime authority and ownership are established by the kernel, not TypeScript casts.
export type Handler = (environment: any, context: Context, ...arguments_: any[]) => unknown;
export interface ServiceDefinition {
  create?: (self: Declaration) => unknown;
  methods: Record<string, Handler>;
  lifetime?: Lifetime;
  linger?: number;
  access?: (principal: string) => { methods: readonly string[] };
}

export interface Requirement {
  facet?: string;
  apiVersion: number;
  methods?: readonly string[];
  services?: Readonly<Record<string, readonly string[]>>;
  hooks?: readonly string[];
}

export interface HookDefinition {
  capabilities?: readonly string[];
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

export interface FacetDefinition {
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
  open(signal: AbortSignal): Completion<T>;
  close?(value: T, context: Context): Completion<unknown>;
}

export interface Resource<T> {
  readonly value: T;
  readonly version: number;
}
export interface TaskScope {
  readonly signal: AbortSignal;
  root<T>(operation: (context: Context) => Completion<T>): Promise<T>;
}
export interface EventSource<T> {
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
  release(): void;
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
  facets: readonly string[];
}
export interface PreparedReload {
  readonly report: ReloadReport;
  readonly status: ReloadStatus;
  commit(): Promise<ReloadReport>;
  abort(): Promise<void>;
}

export interface NativeAdapter {
  hasFacet(id: number): boolean;
  activate(
    object: number,
    facet: number,
    service: string,
    key: readonly string[],
    principal: string,
  ): Promise<void>;
  invoke(
    invocation: Invocation,
    arguments_: readonly unknown[],
    context: Context,
  ): Promise<unknown>;
  dispose(object: number): Promise<void>;
  effect(effect: import("./kernel.ts").Effect): Promise<number | void>;
  /** Signals cancellation without acknowledging physical effect completion. */
  cancelEffect?(effect: import("./kernel.ts").Effect): Completion<void>;
  value(id: number): unknown;
  dropValue?(id: number): void;
}

export interface HostOptions {
  name?: string;
  principal?: string;
  kernel: KernelPort;
  serialization?: Codec;
  load?: (location: string | URL) => Promise<FacetDefinition>;
  onError?: (error: Error) => void;
  now?: () => number;
  native?: NativeAdapter;
  resources?: Readonly<Record<string, ResourceOptions<unknown>>>;
  telemetry?: { capacity?: number; sampleEvery?: number };
}

export interface FacetDescription extends KernelFacet {
  describe(): Readonly<KernelFacet>;
}
