import type {
  Completion,
  DynamicReference,
  EventSource,
  OrbDefinition,
  ListenerOptions,
  Resource,
  ResourceOptions,
  TaskScope,
} from "../authoring/types.ts";
import type { KernelOrb, Owner } from "../bridge/kernel.ts";
import { EnvironmentPhase } from "../bridge/protocol.ts";
import type { StateBinding, StateOptions } from "../state.ts";
import type { Context } from "./context.ts";

export interface OrbRecord {
  info: KernelOrb;
  definition: OrbDefinition;
  scope?: Environment;
  ticket?: number | undefined;
}
export interface Address {
  service: string;
  key: string[];
  principal: string;
}
export interface ObjectRecord {
  id: number;
  address: Address;
  orb: number;
  versions: Map<number, Environment>;
  activation: Promise<void>;
  live?: boolean;
  binding?: StateBinding<any>;
  error?: unknown;
  disposing?: Promise<void>;
}
export interface Environment {
  id: number;
  owner: Owner;
  orb: OrbRecord;
  object?: ObjectRecord;
  phase: EnvironmentPhase;
  value: any;
  dependencies: Map<string, DynamicReference>;
  resources: Map<string, ResourceRecord>;
  resourceDeclarations?: Map<ResourceRecord, ResourceOptions<any>>;
  tasks: TaskDefinition[];
  listeners: ListenerDefinition[];
  state?: {
    initial: any;
    options: StateOptions<any>;
    binding: StateBinding<any>;
    publish?: () => void;
  };
}
export interface ResourceRecord {
  id: number;
  name: string;
  options: ResourceOptions<any>;
  handle: Resource<any>;
  dependencies: number[];
  /** Share one physical resource-open operation. */
  start(): Promise<void>;
  started?: Promise<void>;
  controller: AbortController;
  value?: number;
  closed?: Promise<void>;
  /** Rebind a restored resource to its new kernel identity. */
  reset(id: number): void;
}
export interface TaskDefinition {
  /** Run the task callback under its scope's cancellation signal. */
  start(scope: TaskScope): Completion<void>;
  running?: TaskRecord;
}
export interface TaskRecord {
  id: number;
  controller: AbortController;
  done: Promise<void>;
}
export interface ListenerDefinition {
  name: string;
  source: EventSource<any> | (() => EventSource<any>);
  handle: (context: Context, value: any) => unknown;
  options: ListenerOptions;
  running?: ListenerRecord;
}
export interface ListenerRecord {
  id: number;
  /** Remove the source subscription. */
  detach(): void;
  done?: Promise<void>;
  definition: ListenerDefinition;
  environment: Environment;
  stopped: boolean;
}
export interface ReferenceRecord {
  id: number;
  object: number;
  address: Address;
  ready: Promise<void>;
  methods: Set<string>;
  requiredMethods: ReadonlySet<string>;
  stateCleanups: Set<() => void>;
  nativeOwned?: boolean;
  context?: Context;
  released: boolean;
}

/** Create a promise with explicit completion functions. */
export function deferred<T>(): {
  promise: Promise<T>;
  /** Settle the shared completion with its result. */
  resolve(value: T): void;
  /** Reject the shared completion without an unhandled rejection. */
  reject(error: unknown): void;
} {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((accept, refuse) => {
    resolve = accept;
    reject = refuse;
  });
  promise.catch(() => {});
  return { promise, resolve, reject };
}
