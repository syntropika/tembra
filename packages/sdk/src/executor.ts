import type { Static, TSchema } from "typebox";

import type {
  ContextHandle,
  ContractReference,
  Payload,
  Readable,
  ReferenceOf,
} from "./binding.js";
import { fromDescriptor } from "./contracts.js";
import type {
  AnyContract,
  Infer,
  InputOf,
  LocalContract,
  Method,
  Opaque,
  RemoteContract,
  VariantDefinition,
  Descriptor,
} from "./contracts.js";
import { ContractError } from "./error.js";
import { canonicalJson, parsePortableJson } from "./portable.js";
import type { JsonValue } from "./portable.js";

/** Callback completion may be synchronous or PromiseLike; the executor owns its actual settlement. */
export type Completion<T> = T | PromiseLike<T>;

/** Read-only notification; cancellation never establishes physical completion. */
export type CancellationSignal = {
  readonly aborted: boolean;
  readonly reason: unknown;
  /** Resolves when cancellation is requested, not when owned physical work has finished. */
  readonly cancelled: Promise<void>;
};

/**
 * A backend-issued capability. Structural TypeScript values confer no authority.
 * Cancellation is cooperative: an admitted callback may return a late value. New Host
 * operations still validate the Context, and physical work retains its existing owner.
 */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type ManagedContext = {
  readonly principal: JsonValue;
  /** Version snapshot of this admitted operation; retained Contexts do not become current roots. */
  readonly codeVersion: number;
  readonly signal: CancellationSignal;
  /** Throws when this issued scope no longer admits operations. */
  check(): void;
  /** Requests cooperative cancellation without terminating the callback or undoing its effects. */
  cancel(reason?: string): void;
  /** Acquires a managed reference and awaits its requested readiness within this scope. */
  get<C extends AnyContract>(
    contract: C,
    input: InputOf<C>,
    options?: AcquisitionOptions,
  ): Promise<ReferenceOf<C>>;
} & ContextHandle;
/* oxlint-enable typescript/method-signature-style */

/** Controls acquisition readiness, without changing the provider contract. */
export type AcquisitionOptions = {
  /** Defaults to true: await initial state hydration. False requests shape-only readiness. */
  readonly state?: boolean;
};

/** One core-observed cell revision and value; revision is an observation, not mutation authority. */
export type Snapshot<T> = {
  readonly revision: number;
  readonly value: T;
};

/** Mutable candidate supplied only during a synchronous Cell.change call. */
export type Draft<T> = {
  value: T;
};

/** A weak core cell handle; no state or ownership is implemented in this interface. */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type ManagedReadable<T> = {
  /** A checked initializer Context may read this transaction's private candidate view. */
  snapshot(context?: ContextHandle): Snapshot<T>;
} & Readable<T>;
/* oxlint-enable typescript/method-signature-style */

/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** Core-owned state with checked writes; refused candidate validation leaves committed state intact. */
export type Cell<T> = {
  readonly readable: ManagedReadable<T>;
  /** Validates and commits a replacement under an issued Context; diff requests a differential update. */
  replace(context: ContextHandle, value: T, options?: { readonly diff?: boolean }): Snapshot<T>;
  /** Mutates only a private candidate; the executor rejects asynchronous callbacks. */
  change(context: ContextHandle, mutate: (candidate: Draft<T>) => undefined): Snapshot<T>;
} & ManagedReadable<T>;
/* oxlint-enable typescript/method-signature-style */

/** Stable named cell declaration. Supply exactly one of initial or an awaited initializer. */
export type CellDeclaration<S extends TSchema> = {
  readonly schema: S;
  /** Declaration version, default 1; a change selects migration during reload preparation. */
  readonly version?: number;
  /** Synchronous migration from the prior declaration version, including version jumps. */
  readonly migrate?: (value: unknown, previousVersion: number) => Static<S>;
} & (
  | { readonly initial: Static<S>; readonly initialize?: never }
  | {
      readonly initial?: never;
      readonly initialize: (context: ManagedContext) => Completion<Static<S>>;
    }
);

/** Issued local resource capability. Retaining this handle does not extend its owning object lifetime. */
export type Resource<T> = {
  /** Core refuses access during every factory pass and while the resource is closed. */
  readonly value: T;
  readonly version: number;
  readonly exclusive: boolean;
} & ExplicitCapability<T>;

/** Managed claim retained while a resource is open; plain objects cannot substitute for issued handles. */
export type ResourceDependency = ContractReference | Resource<unknown>;
/** Core-owned acquisition and cleanup; declaration alone never opens a resource during a factory pass. */
export type ResourceDeclaration<T> = {
  /** Declaration version; defaults to 1. */
  readonly version?: number;
  /** Defaults to false; true requires exclusive resource handoff during replacement. */
  readonly exclusive?: boolean;
  /** Managed dependencies retained by the resource owner; defaults to an empty list. */
  readonly deps?: readonly ResourceDependency[];
  /** Awaited acquisition; observe signal cooperatively and release external effects on failure yourself. */
  readonly open: (signal: CancellationSignal) => Completion<T>;
  /** Awaited through actual settlement; its resolved return value is discarded. */
  readonly close?: (value: T) => Completion<unknown>;
};

/** External event subscription data; this structural shape grants no Host authority. */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type EventSource<T> = {
  subscribe(listener: (value: T) => void): () => void;
};
/* oxlint-enable typescript/method-signature-style */

/** Bounds queued events while the managed listener is busy; exceeding the limit refuses admission. */
export type ListenerOptions = {
  /** Checked pending-event limit; defaults to 64, with a maximum of 1,024. */
  readonly maxPending?: number;
};

/** A managed task starts each independent operation in a fresh current-version root. */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type TaskScope = {
  readonly signal: CancellationSignal;
  root<T>(callback: (context: ManagedContext) => Completion<T>): Promise<T>;
};
/* oxlint-enable typescript/method-signature-style */

/** Host-independent metadata; the backend validates and attaches the definition. */
export type Registry = {
  readonly name: string;
  readonly conflict: "error" | "last";
};
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** Callable entry owned by a registry contribution; an issued Context selects its retained version. */
export type RegisteredOperation = {
  call(context: ContextHandle, args: Payload): Promise<Payload>;
};
/* oxlint-enable typescript/method-signature-style */
/** Immutable contribution data and optional managed operation from the same version table. */
export type RegistryEntry = {
  readonly data: JsonValue;
  readonly operation: RegisteredOperation | undefined;
};
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** Retained immutable version table; later registry updates do not replace this snapshot. */
export type RegistrySnapshot = {
  readonly codeVersion: number;
  readonly entries: readonly (readonly [string, RegistryEntry])[];
  get(key: string): RegistryEntry | undefined;
};
/* oxlint-enable typescript/method-signature-style */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** Live registry access; subscriptions must be removed by their caller. */
export type RegistryView = {
  /** Uses the issued Context's version, or the current table when no Context is supplied. */
  table(context?: ContextHandle): RegistrySnapshot;
  /** Ordinary synchronous return values are discarded; PromiseLike returns are refused. */
  subscribe(listener: (snapshot: RegistrySnapshot) => void): () => void;
};
/* oxlint-enable typescript/method-signature-style */

/** Retainable weak Host authority; run cancellation does not prohibit cleanup roots. */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type FacetScope = {
  readonly hostName: string;
  readonly signal: CancellationSignal;
  root<T>(callback: (context: ManagedContext) => Completion<T>): Promise<T>;
  registry(definition: Registry): RegistryView;
};
/* oxlint-enable typescript/method-signature-style */

/** Options are interpreted and checked by the actual core, not by defineFacet. */
export type ServeOptions<P extends AnyContract = AnyContract> = {
  /** Defaults to "ref" (on demand); "host" activates eagerly and permits registry contributions. */
  readonly lifetime?: "host" | "ref";
  /** Idle retention in nonnegative milliseconds; defaults to 0. Checked by the backend. */
  readonly linger?: number;
  /** Synchronous legacy views; the core validates each actual consumer binding. */
  readonly accepts?: readonly ContractAdapter<P>[];
};

declare const capabilityType: unique symbol;
/** Minted by a checked nominal adapter; no structural value is runtime authority. */
export type ExplicitCapability<T = unknown> = {
  readonly [capabilityType]: (value: T) => T;
};

type SourceValues<S extends readonly Readable<unknown>[]> = {
  readonly [K in keyof S]: S[K] extends Readable<infer T> ? Snapshot<T> : never;
};

/**
 * Available only in its exact synchronous object factory pass. Declare stable named cells,
 * resources and activities here; return the implementation synchronously. Retaining this recorder
 * for a later callback or returning a Promise is refused by the backend.
 */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type ObjectDeclaration = {
  readonly principal: JsonValue;
  cell<const S extends TSchema>(name: string, declaration: CellDeclaration<S>): Cell<Static<S>>;
  get<C extends AnyContract>(
    contract: C,
    input: InputOf<C>,
    options?: AcquisitionOptions,
  ): ReferenceOf<C>;
  resource<T>(name: string, declaration: ResourceDeclaration<T>): Resource<T>;
  /** Computes validated state synchronously from core snapshots; PromiseLike results are refused. */
  derive<const S extends readonly Readable<unknown>[], const T extends TSchema>(
    sources: S,
    schema: T,
    compute: (snapshots: SourceValues<S>) => Static<T>,
  ): ManagedReadable<Static<T>>;
  on<T>(
    name: string,
    source: Readable<T>,
    handler: (context: ManagedContext, snapshot: Snapshot<T>) => Completion<void>,
    options?: ListenerOptions,
  ): void;
  onEvent<T>(
    name: string,
    source: EventSource<T>,
    handler: (context: ManagedContext, value: T) => Completion<void>,
    options?: ListenerOptions,
  ): void;
  onResource<T, E>(
    name: string,
    resource: Resource<T>,
    select: (value: T) => EventSource<E>,
    handler: (context: ManagedContext, value: E) => Completion<void>,
    options?: ListenerOptions,
  ): void;
  /** Declares owned background work; its callback must cooperate with task cancellation. */
  task(start: (scope: TaskScope) => Completion<void>): void;
  register(registry: Registry, key: string, data: JsonValue): void;
  registerMethod(registry: Registry, key: string, data: JsonValue, member: string): void;
};
/* oxlint-enable typescript/method-signature-style */

type MethodHandler<M> =
  M extends Method<infer A, infer R>
    ? [A] extends [undefined]
      ? (context: ManagedContext) => Completion<Infer<R>>
      : (context: ManagedContext, args: Infer<A>) => Completion<Infer<R>>
    : never;

type Methods<M> = {
  readonly [K in keyof M as M[K] extends Method<unknown, unknown> ? K : never]: MethodHandler<M[K]>;
};
type StateMember<S> = S extends { readonly kind: "method" | "opaque" }
  ? never
  : S extends TSchema
    ? Readable<Static<S>>
    : never;
type States<M> = {
  readonly [K in keyof M as StateMember<M[K]> extends never ? never : K]: StateMember<M[K]>;
};
type Capabilities<M> = {
  readonly [
    K in keyof M as M[K] extends { readonly kind: "opaque"; readonly id: string } ? K : never
  ]: M[K] extends Opaque<infer T> ? ExplicitCapability<T> : never;
};
type EmptyChecked<M> = keyof M extends never ? Readonly<Record<string, never>> : M;
type ImplementationMembers<M> = {
  readonly methods: EmptyChecked<Methods<M>>;
  readonly state: EmptyChecked<States<M>>;
} & (keyof Capabilities<M> extends never
  ? { readonly opaque?: Readonly<Record<string, never>> }
  : { readonly opaque: Capabilities<M> });

type VariantImplementation<V extends VariantDefinition> = {
  readonly methods: V["methods"] extends object
    ? { readonly [K in keyof V["methods"]]: MethodHandler<V["methods"][K]> }
    : Readonly<Record<string, never>>;
  readonly state: V["state"] extends object
    ? {
        readonly [K in keyof V["state"]]: V["state"][K] extends TSchema
          ? Readable<Static<V["state"][K]>>
          : never;
      }
    : Readonly<Record<string, never>>;
};

/** Provider members inferred from authoring tokens; generated consumer-only tokens lack this metadata. */
export type ImplementationOf<C> =
  C extends LocalContract<TSchema, infer M>
    ? ImplementationMembers<M> & { readonly variant?: never }
    : C extends RemoteContract<TSchema, infer V>
      ? { [K in keyof V]: VariantImplementation<V[K]> & { readonly variant: K } }[keyof V]
      : never;

/** Synchronous factory receives normalized input, then its scoped declaration recorder. */
export type ObjectFactory<C> = C extends AnyContract
  ? (input: InputOf<C>, declaration: ObjectDeclaration) => ImplementationOf<C>
  : never;

declare const contractAdapterBrand: unique symbol;
/** A typed declaration, not a Host capability; backends recheck its full descriptors. */
export type ContractAdapter<P extends AnyContract> = {
  readonly [contractAdapterBrand]: (provider: P) => P;
};

/** Executable callbacks travel separately from portable serve flags.
 * @internal
 */
export type RawContractAdapter = {
  readonly provider: string;
  readonly consumer: string;
  readonly adapt: (current: unknown) => unknown;
};

function adapterRecord(value: unknown, reason: string) {
  if (
    value === null ||
    typeof value !== "object" ||
    Array.isArray(value) ||
    (Object.getPrototypeOf(value) !== Object.prototype && Object.getPrototypeOf(value) !== null)
  ) {
    throw new ContractError(reason, "expected an ordinary data record");
  }
  const record: Record<string, unknown> = {};
  Object.setPrototypeOf(record, null);
  for (const key of Reflect.ownKeys(value)) {
    const property = Object.getOwnPropertyDescriptor(value, key);
    if (
      typeof key !== "string" ||
      property === undefined ||
      !("value" in property) ||
      property.enumerable !== true
    ) {
      throw new ContractError(reason, "expected enumerable data properties");
    }
    record[key] = property.value;
  }
  return record;
}

function checkedToken(value: AnyContract): Descriptor {
  const property =
    value !== null && typeof value === "object"
      ? Object.getOwnPropertyDescriptor(value, "descriptor")
      : undefined;
  if (property === undefined || !("value" in property)) {
    throw new ContractError("accepts", "expected a contract token with its own descriptor");
  }
  return fromDescriptor(property.value).descriptor;
}

function adapterPair(provider: Descriptor, consumer: Descriptor): void {
  if (
    provider.name !== consumer.name ||
    provider.kind !== consumer.kind ||
    canonicalJson(provider.placement ?? null) !== canonicalJson(consumer.placement ?? null)
  ) {
    throw new ContractError("placement", "adapters cannot change contract name, kind or placement");
  }
  if (provider.version === consumer.version) {
    throw new ContractError("accepts", "same-version contracts use coverage, not an adapter");
  }
}

/**
 * Captures checked provider/consumer descriptors without invoking the synchronous adapter.
 * Returned methods may be asynchronous. The backend refuses PromiseLike adapter results;
 * each resulting view is validated by the core against its actual consumer token. Input
 * identity/default compatibility and acquired native channels remain core binding checks.
 * Throws ContractError for invalid tokens, a non-callable adapter or a moved/same-version pair.
 */
export function contractAdapter<P extends AnyContract, L extends AnyContract>(
  provider: P,
  consumer: L,
  adapt: (current: Readonly<ImplementationOf<NoInfer<P>>>) => ImplementationOf<NoInfer<L>>,
): ContractAdapter<P> {
  const source = checkedToken(provider);
  const target = checkedToken(consumer);
  adapterPair(source, target);
  if (typeof adapt !== "function") {
    throw new ContractError("accepts", "adapter must be callable");
  }
  const representation = Object.freeze({
    provider: canonicalJson(source),
    consumer: canonicalJson(target),
    adapt,
  });
  // SAFETY: checked descriptors and the typed callback define this type-only declaration; the backend rechecks the structural representation without granting authority.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- the nominal declaration brand is erased deliberately across SDK copies
  return representation as unknown as ContractAdapter<P>;
}

/**
 * Splits options from callbacks without invoking factory or adapter callbacks.
 * Refuses unknown flags, malformed adapter records, provider mismatch and duplicate versions.
 * It returns fresh frozen transport records; actual installation and binding remain core-owned.
 * @internal
 */
export function prepareServeOptions<P extends AnyContract>(
  provider: P,
  options: ServeOptions<NoInfer<P>> = {},
): { readonly flags: string; readonly adapters: readonly RawContractAdapter[] } {
  const source = checkedToken(provider);
  const sourceText = canonicalJson(source);
  const fields = adapterRecord(options, "serve_options");
  for (const key of Object.keys(fields)) {
    if (key !== "lifetime" && key !== "linger" && key !== "accepts") {
      throw new ContractError("serve_options", `unknown serve option ${key}`);
    }
  }
  const flags: Record<string, JsonValue> = {};
  Object.setPrototypeOf(flags, null);
  if (Object.hasOwn(fields, "lifetime")) {
    if (fields.lifetime !== "host" && fields.lifetime !== "ref") {
      throw new ContractError("serve_options", "unknown lifetime");
    }
    flags.lifetime = fields.lifetime;
  }
  if (Object.hasOwn(fields, "linger")) {
    const value = fields.linger;
    if (
      typeof value !== "number" ||
      !Number.isFinite(value) ||
      value < 0 ||
      value > Number.MAX_SAFE_INTEGER
    ) {
      throw new ContractError("serve_options", "linger must be nonnegative safe milliseconds");
    }
    flags.linger = value;
  }
  const adapters = Object.hasOwn(fields, "accepts")
    ? prepareAdapters(fields.accepts, sourceText)
    : [];
  return Object.freeze({ flags: canonicalJson(flags), adapters: Object.freeze(adapters) });
}

function isAdapter(value: unknown): value is (current: unknown) => unknown {
  return typeof value === "function";
}
function prepareAdapters(values: unknown, sourceText: string): RawContractAdapter[] {
  const adapters: RawContractAdapter[] = [];
  const versions = new Set<number>();
  {
    if (!Array.isArray(values) || Reflect.ownKeys(values).length !== values.length + 1) {
      throw new ContractError("accepts", "expected a dense adapter array");
    }
    for (let index = 0; index < values.length; index += 1) {
      const property = Object.getOwnPropertyDescriptor(values, String(index));
      if (property === undefined || !("value" in property) || property.enumerable !== true) {
        throw new ContractError("accepts", "expected adapter data entries");
      }
      const value = adapterRecord(property.value, "accepts");
      if (
        Object.keys(value).length !== 3 ||
        typeof value.provider !== "string" ||
        typeof value.consumer !== "string" ||
        !isAdapter(value.adapt)
      ) {
        throw new ContractError("accepts", "expected provider, consumer and adapter function");
      }
      const offered = fromDescriptor(parsePortableJson(value.provider)).descriptor;
      const consumer = fromDescriptor(parsePortableJson(value.consumer)).descriptor;
      if (canonicalJson(offered) !== sourceText) {
        throw new ContractError("accepts_provider", "adapter belongs to another provider");
      }
      adapterPair(offered, consumer);
      if (versions.has(consumer.version)) {
        throw new ContractError("accepts", "duplicate consumer version");
      }
      versions.add(consumer.version);
      adapters.push(
        Object.freeze({
          provider: sourceText,
          consumer: canonicalJson(consumer),
          adapt: value.adapt,
        }),
      );
    }
  }
  return adapters;
}

/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** Synchronous setup recorder; serve declares providers without running arbitrary async factories. */
export type FacetDeclaration = {
  /** Registers a provider factory. Host lifetime activation is awaited by install; ref lifetime is lazy. */
  serve<C extends AnyContract>(
    contract: C,
    factory: ObjectFactory<NoInfer<C>>,
    options?: ServeOptions<NoInfer<C>>,
  ): void;
};
/* oxlint-enable typescript/method-signature-style */

/** Executable declaration metadata; setup runs synchronously only when an executor admits it. */
export type FacetDefinition<
  Name extends string = string,
  Uses extends readonly AnyContract[] = readonly AnyContract[],
> = {
  readonly name: Name;
  readonly uses: Uses;
  /** Undefined rejects async setup statically; execution and Promise refusal belong to the executor. */
  readonly setup: (scope: FacetScope, declaration: FacetDeclaration) => undefined;
};

/** Compiled, import-free ESM input. Loaders verify bytes and exports before installation. */
export type EsmArtifactV1 = {
  readonly format: "tembra-esm-v1";
  /** Nonempty diagnostic module identity; this is not a signature or trust assertion. */
  readonly identity: string;
  /** Import-free ESM source, limited to 1 MiB of UTF-8 by the loaders. */
  readonly source: string;
  /** Lowercase hexadecimal SHA-256 of the exact UTF-8 source bytes. */
  readonly sha256: string;
  /** Exact declared named/default export set, verified against the source. */
  readonly exports: readonly string[];
};

/**
 * Core-coordinated reload policy. Deadlines are positive milliseconds, each defaulting to 5,000.
 * Cancellation and deadlines do not establish physical callback completion or undo external effects.
 */
export type ReloadOptions = {
  /** Installed facet names to remove; defaults to none. */
  readonly remove?: readonly string[];
  /** Maximum wait for affected grandfathered roots and physical entries before mutation. */
  readonly gateDeadline?: number;
  /** Retirement budget when an older live generation must leave before admission. */
  readonly retireDeadline?: number;
  /** Cooperative task-stop budget; unfinished physical work remains owned. */
  readonly taskStopDeadline?: number;
  /** An issued notification requests abort before the commit decision only. */
  readonly signal?: CancellationSignal;
};

/** Observations of the completed transaction; these numbers grant no epoch authority. */
export type ReloadReport = {
  readonly codeVersion: number;
  readonly previousVersion: number | undefined;
  readonly changed: readonly string[];
  readonly removed: readonly string[];
  readonly preparedObjects: readonly number[];
  readonly gatedObjects: readonly number[];
  /** Physical operations still owned after their logical retirement/cancellation. */
  readonly lingeringPhysical: number;
};

/**
 * Copies and freezes facet metadata without executing setup or attaching anything to a Host.
 * Contract tokens and the callback retain identity. Core/executor admission remains authoritative.
 * Throws ContractError for an invalid name, non-array uses or a non-callable setup.
 *
 * @example
 * ```ts
 * import { defineFacet, local, method, Type } from "@tembra/sdk";
 * const Counter = local("counter", {
 *   version: 1, input: Type.Object({}),
 *   members: { count: Type.Integer(), increment: method(Type.Integer(), Type.Integer()) },
 * });
 * const facet = defineFacet({
 *   name: "counter-provider", uses: [Counter],
 *   setup(_scope, declaration) {
 *     declaration.serve(Counter, (_input, self) => {
 *       const count = self.cell("count", { schema: Type.Integer(), initial: 0 });
 *       return {
 *         state: { count },
 *         methods: { increment: (context, amount) => count.replace(context, count.value + amount).value },
 *       };
 *     });
 *   },
 * });
 * // Pass facet to the embedding executor's install/reload operation.
 * ```
 */
export function defineFacet<const Name extends string, const Uses extends readonly AnyContract[]>(
  definition: FacetDefinition<Name, Uses>,
): FacetDefinition<Name, Readonly<Uses>> {
  const { name, uses, setup } = definition;
  if (typeof name !== "string" || !name || /\p{Cc}/u.test(name)) {
    throw new ContractError("facet_name", "facet name must be nonempty without control characters");
  }
  if (!Array.isArray(uses)) {
    throw new ContractError("facet_uses", "facet uses must be an array of contract tokens");
  }
  if (typeof setup !== "function") {
    throw new ContractError("facet_setup", "facet setup must be callable");
  }
  // SAFETY: spreading preserves the checked array's tuple length/order and every contract identity.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- TypeScript loses generic tuple evidence when copying an array
  const copied = Object.freeze([...uses]) as Readonly<Uses>;
  return Object.freeze({ name, uses: copied, setup });
}
