import { EnvironmentPhase } from "../protocol.ts";
import type { KernelFacet, Owner } from "../kernel.ts";
import type { StateBinding, StateOptions } from "../state.ts";
import type { Context } from "./context.ts";
import type {
  Completion,
  DynamicReference,
  EventSource,
  FacetDefinition,
  ListenerOptions,
  Resource,
  ResourceOptions,
  TaskScope,
} from "../types.ts";

export interface FacetRecord {
  info: KernelFacet;
  definition: FacetDefinition;
  scope?: Environment;
}
export interface Address {
  service: string;
  key: string[];
  principal: string;
}
export interface ObjectRecord {
  id: number;
  address: Address;
  facet: number;
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
  facet: FacetRecord;
  object?: ObjectRecord;
  phase: EnvironmentPhase;
  value: any;
  dependencies: Map<string, DynamicReference>;
  resources: Map<string, ResourceRecord>;
  tasks: TaskDefinition[];
  listeners: ListenerDefinition[];
  state?: {
    initial: any;
    options: StateOptions<any>;
    handle: unknown;
    binding: StateBinding<any>;
    publish?: () => void;
  };
}
export interface ResourceRecord {
  id: number;
  name: string;
  owner: Owner;
  options: ResourceOptions<any>;
  handle: Resource<any>;
  dependencies: number[];
  ready: Promise<void>;
  start(): Promise<void>;
  started?: Promise<void>;
  controller: AbortController;
  value?: number;
  closed?: Promise<void>;
  exclusive: boolean;
  version: number;
  reset(id: number): void;
}
export interface TaskDefinition {
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
  source: EventSource<any>;
  detach(): void;
  done?: Promise<void>;
  definition: ListenerDefinition;
  environment: Environment;
  stopped: boolean;
  active: boolean;
}
export interface ReferenceRecord {
  id: number;
  object: number;
  address: Address;
  ready: Promise<void>;
  methods: Set<string>;
  requiredMethods: ReadonlySet<string>;
  stateCleanups: Set<() => void>;
  context?: Context;
  released: boolean;
  dependency?: boolean;
}

export function deferred<T>(): {
  promise: Promise<T>;
  resolve(value: T): void;
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
