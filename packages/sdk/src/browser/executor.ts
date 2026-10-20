// oxlint-disable max-classes-per-file -- The private local binding port and public executor share issued-handle authority maps.
import { bindReference, canonicalJson, normalizeSchema, prepareServeOptions } from "../index.js";
import type {
  AcquisitionOptions,
  AnyContract,
  BindingPort,
  CallRequest,
  Descriptor,
  CancellationSignal,
  Cell,
  CellDeclaration,
  ContextHandle,
  EsmArtifactV1,
  FacetDeclaration,
  FacetDefinition,
  FacetScope,
  InputOf,
  JsonValue,
  ManagedContext,
  MemberDescriptor,
  ObjectDeclaration,
  ObjectFactory,
  Payload,
  Readable,
  ReferenceOf,
  ManagedReadable,
  EventSource,
  ListenerOptions,
  TaskScope,
  ReloadOptions,
  ReloadReport,
  RefStatus,
  Registry,
  RegistryEntry,
  RegistrySnapshot,
  RegistryView,
  Resource,
  ResourceDeclaration,
  ServeOptions,
  Shape,
  Snapshot,
  Static,
  TSchema,
} from "../index.js";
import type { WasmMethod } from "./abi.js";
import { evaluateEsmArtifact, executorFailure, realmDiagnostics } from "./artifact.js";
import type { BrowserClient, WasmHost, WasmSubscription } from "./index.js";

/** Raw checked executor ABI for an existing Rust Host; free its wrapper only after owned work settles. */
export type WasmExecutorPort = {
  install: WasmMethod<
    [
      name: string,
      uses: string,
      setup: (scope: RawScope, declaration: RawFacetRecorder) => undefined,
    ],
    Promise<void>
  >;
  reserveGeneration: WasmMethod<[bytes: number], WasmGenerationPort>;
  prepare: WasmMethod<
    [
      name: string,
      uses: string,
      setup: (scope: RawScope, declaration: RawFacetRecorder) => undefined,
    ],
    RawCandidate
  >;
  beginReload: WasmMethod<[options: string], RawReloadBuilder>;
  diagnosticsJson: WasmMethod<[], string>;
  free: WasmMethod<[], void>;
};
type WasmGenerationPort = {
  install: WasmMethod<
    [
      name: string,
      uses: string,
      setup: (scope: RawScope, declaration: RawFacetRecorder) => undefined,
    ],
    Promise<void>
  >;
  prepare: WasmMethod<
    [
      name: string,
      uses: string,
      setup: (scope: RawScope, declaration: RawFacetRecorder) => undefined,
    ],
    RawCandidate
  >;
  free: WasmMethod<[], void>;
};
type RawCandidate = { free: WasmMethod<[], void> };
type RawReloadBuilder = {
  add: WasmMethod<[candidate: RawCandidate], void>;
  signal: WasmMethod<[signal: RawSignal], void>;
  start: WasmMethod<[], Promise<string>>;
  free: WasmMethod<[], void>;
};
/** Initialized WASM module exposing the checked executor constructor; shares the client module instance. */
export type ExecutorWasmModule = {
  createCheckedWasmExecutor: WasmMethod<[host: WasmHost], WasmExecutorPort>;
};
type RawSignal = {
  aborted: WasmMethod<[], boolean>;
  reasonJson: WasmMethod<[], string>;
  cancelled: WasmMethod<[], Promise<void>>;
};
type RawContext = {
  token: WasmMethod<[], unknown>;
  check: WasmMethod<[], void>;
  cancel: WasmMethod<[reason: string], void>;
  principalJson: WasmMethod<[], string>;
  codeVersion: WasmMethod<[], number>;
  signal: WasmMethod<[], RawSignal>;
  get: WasmMethod<[descriptor: string, input: string, state: boolean], Promise<RawReference>>;
};
type RawScope = {
  hostName: WasmMethod<[], string>;
  signal: WasmMethod<[], RawSignal>;
  root: WasmMethod<[callback: (context: RawContext) => unknown], Promise<unknown>>;
  registry: WasmMethod<[definition: string], RawRegistryView>;
};
type RawTaskScope = {
  signal: WasmMethod<[], RawSignal>;
  root: WasmMethod<[callback: (context: RawContext) => unknown], Promise<unknown>>;
};
type RawFacetRecorder = {
  adapterVersion?: WasmMethod<[], number>;
  serve: WasmMethod<
    [
      descriptor: string,
      factory: (input: string, principal: string, declaration: RawObject) => RawImplementation,
      options: string,
      adapters?: readonly RawAdapter[],
    ],
    void
  >;
};
type RawProjection = {
  exposeState: WasmMethod<[name: string, state: RawState], void>;
  exposeOpaque: WasmMethod<[name: string, capability: RawCapability], void>;
};
type RawAdapter = {
  readonly provider: string;
  readonly consumer: string;
  readonly adapt: (current: unknown, output: RawProjection) => RawImplementation;
};
type RawImplementation = {
  readonly variant?: string;
  readonly methods: Record<string, (context: RawContext, input: unknown) => unknown>;
  readonly aliases?: Readonly<Record<string, string>>;
  readonly projection?: unknown;
};
type RawObject = {
  cell: WasmMethod<
    [
      name: string,
      schema: string,
      initial: string | undefined,
      initialize: ((context: RawContext) => Promise<string>) | undefined,
      version: number,
      migrate: ((value: string, previousVersion: number) => string) | undefined,
    ],
    RawCell
  >;
  exposeState: WasmMethod<[name: string, state: RawState], void>;
  exposeOpaque: WasmMethod<[name: string, capability: RawCapability], void>;
  get: WasmMethod<[descriptor: string, input: string, state: boolean], RawReference>;
  beginResource: WasmMethod<
    [
      open: (signal: RawSignal) => unknown,
      close: ((value: unknown) => unknown) | undefined,
      flags: string,
    ],
    RawResourceBuilder
  >;
  derive: WasmMethod<[schema: string, compute: (values: string) => string], RawDerivationBuilder>;
  register: WasmMethod<
    [registry: string, key: string, data: string, member: string | undefined],
    void
  >;
  onState: WasmMethod<
    [
      name: string,
      source: RawState,
      handler: (context: RawContext, snapshot: unknown) => unknown,
      options: string,
    ],
    void
  >;
  onEvent: WasmMethod<
    [
      name: string,
      source: EventSource<unknown>,
      handler: (context: RawContext, value: unknown) => unknown,
      options: string,
    ],
    void
  >;
  onResource: WasmMethod<
    [
      name: string,
      resource: RawResource,
      select: (value: unknown) => EventSource<unknown>,
      handler: (context: RawContext, value: unknown) => unknown,
      options: string,
    ],
    void
  >;
  task: WasmMethod<[start: (scope: RawTaskScope) => unknown], void>;
};
type RawState = {
  snapshotJson: WasmMethod<[], string>;
  snapshotInJson: WasmMethod<[token: unknown], string>;
  subscribe: WasmMethod<[callback: (snapshot: string) => unknown], WasmSubscription>;
};
type RawCell = {
  state: WasmMethod<[], RawState>;
  replace: WasmMethod<[token: unknown, value: string, diff: boolean], string>;
  change: WasmMethod<[token: unknown, change: (value: string) => string], string>;
};
type RawResource = {
  value: WasmMethod<[], unknown>;
  version: WasmMethod<[], number>;
  exclusive: WasmMethod<[], boolean>;
  capability: WasmMethod<[nominal: string], RawCapability>;
};
type RawCapability = { readonly __capability?: never };
type RawResourceBuilder = {
  referenceDependency: WasmMethod<[reference: RawReference], void>;
  resourceDependency: WasmMethod<[resource: RawResource], void>;
  finish: WasmMethod<[name: string], RawResource>;
  free: WasmMethod<[], void>;
};
type RawDerivationBuilder = {
  source: WasmMethod<[state: RawState], void>;
  finish: WasmMethod<[], RawState>;
  free: WasmMethod<[], void>;
};
type RawReference = {
  statusJson: WasmMethod<[], string>;
  shapeJson: WasmMethod<[], string>;
  errorJson: WasmMethod<[], string>;
  ready: WasmMethod<[], Promise<void>>;
  call: WasmMethod<
    [member: string, input: unknown, token: unknown, native: boolean],
    Promise<unknown>
  >;
  opaque: WasmMethod<[member: string], unknown>;
  state: WasmMethod<[member: string], RawState>;
  release: WasmMethod<[], void>;
  onStatus: WasmMethod<[listener: (value: string) => unknown], WasmSubscription>;
  onShape: WasmMethod<[listener: (value: string) => unknown], WasmSubscription>;
};
type RawRegistryView = {
  table: WasmMethod<[token: unknown], RawRegistrySnapshot>;
  subscribe: WasmMethod<[listener: (value: RawRegistrySnapshot) => unknown], WasmSubscription>;
};
type RawRegistrySnapshot = {
  entriesJson: WasmMethod<[], string>;
  operation: WasmMethod<[key: string], RawOperation | undefined>;
};
type RawOperation = {
  metadataJson: WasmMethod<[], string>;
  call: WasmMethod<[token: unknown, input: unknown, native: boolean], Promise<unknown>>;
};

const { apply } = Reflect;
const PromiseConstructor = Promise;
// oxlint-disable-next-line typescript/unbound-method -- Reflect.apply below supplies the captured Promise constructor receiver.
const { resolve } = Promise;
// oxlint-disable-next-line typescript/unbound-method,promise/spec-only -- The captured intrinsic is intentionally invoked with its original promise receiver through Reflect.apply.
const { then } = Promise.prototype;
const freezeObject = Object.freeze;
// oxlint-disable-next-line typescript/promise-function-async -- The captured intrinsic observer must not add another promise adoption boundary.
function observe(value: unknown): Promise<unknown> {
  const promise = apply(resolve, PromiseConstructor, [value]);
  return new PromiseConstructor((ok, fail) => {
    // oxlint-disable-next-line typescript/no-floating-promises -- Both captured handlers are installed; native resolve/reject cannot throw, so the returned observer promise fulfills.
    apply(then, promise, [ok, fail]);
  });
}
function invoke(callback: unknown, args: unknown[], receiver?: unknown): unknown {
  if (typeof callback !== "function") {
    throw executorFailure("callback", "expected a callable");
  }
  return apply(callback, receiver, args);
}
function sync(value: unknown): unknown {
  if (
    value !== null &&
    (typeof value === "object" || typeof value === "function") &&
    typeof Reflect.get(value, "then") === "function"
  ) {
    throw executorFailure("async_declaration", "callback must return synchronously");
  }
  return value;
}
function voidResult(value: unknown): undefined {
  if (sync(value) !== undefined) {
    throw executorFailure("callback_result", "callback must return undefined");
  }
  return undefined;
}
function checkedFacet(value: Record<string, unknown>): FacetDefinition {
  if (
    typeof value.name !== "string" ||
    !Array.isArray(value.uses) ||
    typeof value.setup !== "function"
  ) {
    throw executorFailure("facet", "selected export is not a facet definition");
  }
  // SAFETY: the three guards establish the same executable facet fields consumed by setupShim; contract descriptors are checked by Rust admission.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- This is the guarded executable facet boundary.
  return value as FacetDefinition;
}
function record(value: unknown): Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw executorFailure("implementation", "expected an object");
  }
  // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  return value as Record<string, unknown>;
}
// oxlint-disable-next-line typescript/no-unnecessary-type-parameters -- The issued Rust accessor determines this serialized return type at each call site.
function detached<T>(text: string): T {
  // oxlint-disable-next-line unicorn/consistent-function-scoping -- The recursive freeze preserves the input generic type and belongs to this ABI decoder.
  function freeze<TValue>(value: TValue): TValue {
    if (value !== null && typeof value === "object") {
      for (const child of Object.values(value)) {
        freeze(child);
      }
      freezeObject(value);
    }
    return value;
  }
  // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  return freeze(JSON.parse(text) as T);
}
function unsubscribe(subscription: WasmSubscription): () => void {
  let active = true;
  return () => {
    if (active) {
      active = false;
      subscription.unsubscribe();
      subscription.free();
    }
  };
}
function stateDemand(options: AcquisitionOptions = {}): boolean {
  if (
    Object.keys(options).some((key) => key !== "state") ||
    (options.state !== undefined && typeof options.state !== "boolean")
  ) {
    throw executorFailure("options", "only the state acquisition option is accepted");
  }
  return options.state ?? true;
}
function declarationVersion(value: number | undefined): number {
  if (value === undefined) {
    return 1;
  }
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) {
    throw executorFailure("version", "version must be a positive safe integer");
  }
  return value;
}
const signals = new WeakMap<object, RawSignal>();
function wrapSignal(raw: RawSignal): CancellationSignal {
  let wait: Promise<void> | undefined;
  const signal = freezeObject({
    get aborted() {
      return raw.aborted();
    },
    get reason() {
      return detached<unknown>(raw.reasonJson());
    },
    get cancelled() {
      return (wait ??= raw.cancelled());
    },
  });
  signals.set(signal, raw);
  return signal;
}
function wrapContext(raw: RawContext): ManagedContext {
  const signal = wrapSignal(raw.signal());
  return freezeObject({
    runtime: "tembra" as const,
    handle: raw.token(),
    signal,
    get principal() {
      return detached<JsonValue>(raw.principalJson());
    },
    get codeVersion() {
      return raw.codeVersion();
    },
    check: () => {
      raw.check();
    },
    cancel: (reason = "requested") => {
      raw.cancel(reason);
    },
    async get<C extends AnyContract>(contract: C, input: InputOf<C>, options?: AcquisitionOptions) {
      return bind(
        contract,
        await raw.get(
          canonicalJson(contract.descriptor),
          canonicalJson(input),
          stateDemand(options),
        ),
      );
    },
  });
}
const states = new WeakMap<object, RawState>();
const resources = new WeakMap<object, RawResource>();
const references = new WeakMap<object, RawReference>();
function source(value: unknown): RawState {
  const raw = value !== null && typeof value === "object" ? states.get(value) : undefined;
  if (!raw) {
    throw executorFailure("foreign_handle", "state was not issued by this binding");
  }
  return raw;
}
function wrapState<T>(raw: RawState): ManagedReadable<T> {
  const value: ManagedReadable<T> = freezeObject({
    get value() {
      return detached<Snapshot<T>>(raw.snapshotJson()).value;
    },
    snapshot: (context?: ContextHandle) =>
      detached<Snapshot<T>>(
        context === undefined ? raw.snapshotJson() : raw.snapshotInJson(context.handle),
      ),
    subscribe(listener: (value: T) => void) {
      return unsubscribe(
        raw.subscribe((text) => {
          sync(invoke(listener, [detached<Snapshot<T>>(text).value]));
        }),
      );
    },
  });
  states.set(value, raw);
  return value;
}
function wrapCell<T>(raw: RawCell): Cell<T> {
  const state = raw.state();
  const readable = wrapState<T>(state);
  const cell: Cell<T> = freezeObject({
    readable,
    get value() {
      return readable.value;
    },
    subscribe: (listener: (value: T) => void) => readable.subscribe(listener),
    snapshot: (context?: ContextHandle) => readable.snapshot(context),
    replace: (context: ContextHandle, value: T, options: { diff?: boolean } = {}) => {
      if (
        Object.keys(options).some((key) => key !== "diff") ||
        (options.diff !== undefined && typeof options.diff !== "boolean")
      ) {
        throw executorFailure("options", "only boolean diff is accepted");
      }
      return detached<Snapshot<T>>(
        raw.replace(context.handle, canonicalJson(value), options.diff ?? false),
      );
    },
    change: (context: ContextHandle, mutate: (candidate: { value: T }) => undefined) =>
      detached<Snapshot<T>>(
        raw.change(context.handle, (text) => {
          // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
          // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
          const candidate = { value: JSON.parse(text) as T };
          voidResult(invoke(mutate, [candidate]));
          return canonicalJson(candidate.value);
        }),
      ),
  });
  states.set(cell, state);
  return cell;
}
function wrapResource<T>(raw: RawResource): Resource<T> {
  // The private map and Rust WeakResource are the authority, never this assertion/shape.
  // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
  /* oxlint-disable typescript/no-unsafe-type-assertion -- The issued Rust resource and private map below establish authority; the phantom capability property is compile-time only. */
  const resource = freezeObject({
    get value() {
      // SAFETY: the resource ABI returns the same T value produced by the captured opener.
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
      return raw.value() as T;
    },
    get version() {
      return raw.version();
    },
    get exclusive() {
      return raw.exclusive();
    },
  }) as Resource<T>;
  /* oxlint-enable typescript/no-unsafe-type-assertion */
  resources.set(resource, raw);
  return resource;
}
function nativeMember(member: MemberDescriptor): boolean {
  return (
    member.kind === "method" &&
    [member.args, member.result].some(
      (schema) =>
        schema !== undefined &&
        typeof schema === "object" &&
        schema !== null &&
        Object.hasOwn(schema, "native"),
    )
  );
}
function shapeFrom(raw: string): Shape | undefined {
  // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const result = JSON.parse(raw) as
    | { present: false }
    | { present: true; value: Shape & { variant: string | null } };
  return result.present
    ? { ...result.value, variant: result.value.variant ?? undefined }
    : undefined;
}
class LocalPort implements BindingPort {
  constructor(
    readonly contract: AnyContract,
    readonly raw: RawReference,
  ) {}
  get status(): RefStatus {
    // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
    return JSON.parse(this.raw.statusJson()) as RefStatus;
  }
  get shape(): Shape | undefined {
    return shapeFrom(this.raw.shapeJson());
  }
  get variant(): string | undefined {
    return this.shape?.variant;
  }
  get error(): unknown {
    // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
    const e = JSON.parse(this.raw.errorJson()) as { present: boolean; value?: unknown };
    return e.present ? e.value : undefined;
  }
  get ready(): Promise<void> {
    return this.raw.ready();
  }
  async call(request: CallRequest): Promise<Payload> {
    if (request.contract.fingerprint !== this.contract.descriptor.fingerprint) {
      throw executorFailure("contract", "reference contract mismatch");
    }
    const member =
      this.contract.descriptor.variants[this.variant ?? ""]?.[request.member] ??
      Object.values(this.contract.descriptor.variants)
        .map((v) => v[request.member])
        .find((v) => v !== undefined);
    if (!member || member.kind !== "method") {
      throw executorFailure("method", "member is not a method");
    }
    if (nativeMember(member)) {
      const value = await this.raw.call(
        request.member,
        request.args.present ? request.args.value : undefined,
        request.context?.handle,
        true,
      );
      return member.result === undefined ? { present: false } : { present: true, value };
    }
    const result = await this.raw.call(
      request.member,
      canonicalJson(request.args),
      request.context?.handle,
      false,
    );
    if (typeof result !== "string") {
      throw executorFailure("payload", "backend returned a non-text portable payload");
    }
    // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
    return JSON.parse(result) as Payload;
  }
  readable(member: string): Readable<unknown> {
    return wrapState(this.raw.state(member));
  }
  opaque(member: string): unknown {
    return this.raw.opaque(member);
  }
  release(): void {
    this.raw.release();
  }
  onStatus(listener: (status: RefStatus, error: unknown) => void): () => void {
    return unsubscribe(
      this.raw.onStatus((text) => {
        // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
        const value = JSON.parse(text) as { status: RefStatus; error: unknown };
        sync(invoke(listener, [value.status, value.error]));
      }),
    );
  }
  onShape(listener: (shape: Shape) => void): () => void {
    return unsubscribe(
      this.raw.onShape((text) => {
        // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
        const value = JSON.parse(text) as Shape & { variant: string | null };
        sync(invoke(listener, [{ ...value, variant: value.variant ?? undefined }]));
      }),
    );
  }
}
function bind<C extends AnyContract>(contract: C, raw: RawReference): ReferenceOf<C> {
  const value = bindReference(contract, new LocalPort(contract, raw));
  // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  references.set(value as object, raw);
  return value;
}
function wrapRegistry(raw: RawRegistryView): RegistryView {
  const snapshot = (rawValue: RawRegistrySnapshot): RegistrySnapshot => {
    const data = detached<{
      codeVersion: number;
      entries: { key: string; data: JsonValue; operation: boolean }[];
    }>(rawValue.entriesJson());
    const entries: readonly (readonly [string, RegistryEntry])[] = freezeObject(
      data.entries.map((entry) => {
        const operation = entry.operation ? rawValue.operation(entry.key) : undefined;
        const value: RegistryEntry = freezeObject({
          data: entry.data,
          operation:
            operation === undefined
              ? undefined
              : freezeObject({
                  async call(context: ContextHandle, args: Payload): Promise<Payload> {
                    // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
                    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
                    const metadata = JSON.parse(operation.metadataJson()) as MemberDescriptor;
                    const native = nativeMember(metadata);
                    const nativeInput = args.present ? args.value : undefined;
                    const result = await operation.call(
                      context.handle,
                      native ? nativeInput : canonicalJson(args),
                      native,
                    );
                    if (native && metadata.kind === "method") {
                      return metadata.result === undefined
                        ? { present: false }
                        : { present: true, value: result };
                    }
                    if (typeof result !== "string") {
                      throw executorFailure(
                        "payload",
                        "backend returned a non-text portable payload",
                      );
                    }
                    // SAFETY: Rust validates the portable result against this registry operation's contract before serialization.
                    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked ABI returns the payload envelope.
                    return JSON.parse(result) as Payload;
                  },
                }),
        });
        return freezeObject([entry.key, value] as const);
      }),
    );
    return freezeObject({
      codeVersion: data.codeVersion,
      entries,
      get: (key: string) => entries.find(([name]) => name === key)?.[1],
    });
  };
  return freezeObject({
    table: (context?: ContextHandle) => snapshot(raw.table(context?.handle)),
    subscribe: (listener: (snapshot: RegistrySnapshot) => void) =>
      unsubscribe(
        raw.subscribe((value) => {
          sync(invoke(listener, [snapshot(value)]));
        }),
      ),
  });
}
function wrapScope(raw: RawScope): FacetScope {
  return freezeObject({
    hostName: raw.hostName(),
    signal: wrapSignal(raw.signal()),
    async root<T>(operation: (context: ManagedContext) => T | PromiseLike<T>): Promise<T> {
      // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
      return (await raw.root((context) => operation(wrapContext(context)))) as T;
    },
    registry: (definition: Registry) => wrapRegistry(raw.registry(canonicalJson(definition))),
  });
}
function wrapTask(raw: RawTaskScope): TaskScope {
  return freezeObject({
    signal: wrapSignal(raw.signal()),
    async root<T>(operation: (context: ManagedContext) => T | PromiseLike<T>): Promise<T> {
      // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
      return (await raw.root((context) => operation(wrapContext(context)))) as T;
    },
  });
}
type Selector = (value: unknown) => unknown;
const selectors = new WeakMap<Selector, (value: unknown) => EventSource<unknown>>();
function checkedSelector(selector: Selector): (value: unknown) => EventSource<unknown> {
  let wrapped = selectors.get(selector);
  if (wrapped === undefined) {
    // SAFETY: the typed selector or issued-handle association preserves this generic value across the Rust ABI.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
    wrapped = (value) => sync(invoke(selector, [value])) as EventSource<unknown>;
    selectors.set(selector, wrapped);
  }
  return wrapped;
}
function wrapObject(raw: RawObject, principal: string): ObjectDeclaration {
  return freezeObject({
    principal: detached<JsonValue>(principal),
    cell<const S extends TSchema>(name: string, declaration: CellDeclaration<S>): Cell<Static<S>> {
      const schema = canonicalJson(normalizeSchema(declaration.schema));
      const initialize =
        "initialize" in declaration && declaration.initialize !== undefined
          ? async (context: RawContext) =>
              canonicalJson(await observe(declaration.initialize(wrapContext(context))))
          : undefined;
      const initial = Object.hasOwn(declaration, "initial")
        ? canonicalJson(declaration.initial)
        : undefined;
      const migrate =
        declaration.migrate === undefined
          ? undefined
          : (value: string, previousVersion: number) =>
              canonicalJson(sync(invoke(declaration.migrate, [detached(value), previousVersion])));
      return wrapCell(
        raw.cell(
          name,
          schema,
          initial,
          initialize,
          declarationVersion(declaration.version),
          migrate,
        ),
      );
    },
    get<C extends AnyContract>(
      contract: C,
      input: InputOf<C>,
      options?: AcquisitionOptions,
    ): ReferenceOf<C> {
      return bind(
        contract,
        raw.get(canonicalJson(contract.descriptor), canonicalJson(input), stateDemand(options)),
      );
    },
    resource<T>(name: string, declaration: ResourceDeclaration<T>): Resource<T> {
      const builder = raw.beginResource(
        (signal) => declaration.open(wrapSignal(signal)),
        // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
        declaration.close === undefined
          ? undefined
          : (value) => {
              const closer = declaration.close;
              if (closer === undefined) {
                throw executorFailure("resource_close", "declared resource closer was removed");
              }
              // SAFETY: the resource returns the same T produced by its typed opener.
              // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- Rust preserves this resource's opaque value identity.
              return closer.call(declaration, value as T);
            },
        canonicalJson({
          version: declarationVersion(declaration.version),
          exclusive: declaration.exclusive ?? false,
        }),
      );
      try {
        for (const dependency of declaration.deps ?? []) {
          const resource = resources.get(dependency);
          if (resource) {
            builder.resourceDependency(resource);
          } else {
            const reference = references.get(dependency);
            if (!reference) {
              throw executorFailure("foreign_handle", "dependency was not issued by this binding");
            }
            builder.referenceDependency(reference);
          }
        }
        return wrapResource(builder.finish(name));
      } finally {
        builder.free();
      }
    },
    derive<const S extends readonly Readable<unknown>[], const T extends TSchema>(
      sources: S,
      schema: T,
      compute: (snapshots: {
        readonly [K in keyof S]: S[K] extends Readable<infer V> ? Snapshot<V> : never;
      }) => Static<T>,
    ): ManagedReadable<Static<T>> {
      const builder = raw.derive(canonicalJson(normalizeSchema(schema)), (text) =>
        canonicalJson(sync(invoke(compute, [detached(text)]))),
      );
      try {
        for (const value of sources) {
          builder.source(source(value));
        }
        return wrapState<Static<T>>(builder.finish());
      } finally {
        builder.free();
      }
    },
    on<T>(
      name: string,
      value: Readable<T>,
      handler: (context: ManagedContext, snapshot: Snapshot<T>) => unknown,
      options: ListenerOptions = {},
    ) {
      raw.onState(
        name,
        source(value),
        (context, snapshot) =>
          invoke(handler, [wrapContext(context), detached<Snapshot<T>>(canonicalJson(snapshot))]),
        canonicalJson(options),
      );
    },
    onEvent<T>(
      name: string,
      value: EventSource<T>,
      handler: (context: ManagedContext, value: T) => unknown,
      options: ListenerOptions = {},
    ) {
      raw.onEvent(
        name,
        // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
        value as EventSource<unknown>,
        (context, event) => invoke(handler, [wrapContext(context), event]),
        canonicalJson(options),
      );
    },
    onResource<T, E>(
      name: string,
      value: Resource<T>,
      select: (value: T) => EventSource<E>,
      handler: (context: ManagedContext, value: E) => unknown,
      options: ListenerOptions = {},
    ) {
      const resource = resources.get(value);
      if (!resource) {
        throw executorFailure("foreign_handle", "resource was not issued by this binding");
      }
      raw.onResource(
        name,
        resource,
        // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
        checkedSelector(select as Selector),
        (context, event) => invoke(handler, [wrapContext(context), event]),
        canonicalJson(options),
      );
    },
    task(start: (scope: TaskScope) => unknown) {
      raw.task((scope) => invoke(start, [wrapTask(scope)]));
    },
    register: (registry: Registry, key: string, data: JsonValue) => {
      raw.register(canonicalJson(registry), key, canonicalJson(data), undefined);
    },
    registerMethod: (registry: Registry, key: string, data: JsonValue, member: string) => {
      raw.register(canonicalJson(registry), key, canonicalJson(data), member);
    },
  });
}
function opaqueCapability(member: MemberDescriptor | undefined, value: unknown): RawCapability {
  const resource = value !== null && typeof value === "object" ? resources.get(value) : undefined;
  if (member === undefined || member.kind !== "opaque" || resource === undefined) {
    throw executorFailure("opaque", "expected a minted resource capability");
  }
  return resource.capability(member.id);
}
function implementationVariant(value: unknown): string | undefined {
  if (value === undefined || typeof value === "string") {
    return value;
  }
  throw executorFailure("variant", "variant must be text");
}
function implementationOutput(
  descriptor: Descriptor,
  value: unknown,
  output: RawProjection,
  current?: unknown,
): RawImplementation {
  const implementation = record(sync(value));
  const variant = implementationVariant(implementation.variant);
  // SAFETY: the typed callback or issued-handle association preserves this generic value across the Rust ABI.
  const members = descriptor.variants[variant ?? ""];
  if (!members) {
    throw executorFailure("variant", "unknown variant");
  }
  // SAFETY: the null-prototype map starts empty; only validated entries are inserted below.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const methods: RawImplementation["methods"] = Object.create(null) as RawImplementation["methods"];
  // SAFETY: the null-prototype map starts empty; only validated entries are inserted below.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const aliases: Record<string, string> = Object.create(null) as Record<string, string>;
  // SAFETY: the null-prototype map starts empty; only validated entries are inserted below.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const projectedMethods: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
  const sourceMethods = current === undefined ? undefined : record(record(current).methods);
  const suppliedMethods = record(implementation.methods);
  const methodEntries =
    current === undefined
      ? Object.entries(suppliedMethods)
      : Object.keys(suppliedMethods)
          .filter((name) => members[name]?.kind === "method")
          .map((name) => [name, suppliedMethods[name]] as const);
  for (const [name, callback] of methodEntries) {
    const member = members[name];
    if (!member || member.kind !== "method") {
      throw executorFailure("member", "undeclared method");
    }
    if (typeof callback !== "function") {
      throw executorFailure("callback", "expected a callable");
    }
    // A distinct projection function identifies each source member, even if handlers are shared.
    projectedMethods[name] = (...args: unknown[]) => invoke(callback, args);
    const original =
      sourceMethods &&
      Object.entries(sourceMethods).find(([, sourceValue]) => sourceValue === callback);
    if (original) {
      [aliases[name]] = original;
      continue;
    }
    methods[name] = async (context, input) => {
      const native = nativeMember(member);
      let payload: Payload;
      if (native) {
        payload = { present: member.args !== undefined, value: input };
      } else {
        if (typeof input !== "string") {
          throw executorFailure("payload", "backend returned non-text portable arguments");
        }
        // SAFETY: Rust validates portable invocation arguments against the bound member before serialization.
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked ABI supplies the payload envelope.
        payload = JSON.parse(input) as Payload;
      }
      const args: unknown[] = [wrapContext(context)];
      if (payload.present) {
        args.push(payload.value);
      }
      const result = await observe(invoke(callback, args));
      if (member.result === undefined) {
        return native ? undefined : canonicalJson({ present: false });
      }
      return native
        ? result
        : canonicalJson(
            result === undefined ? { present: false } : { present: true, value: result },
          );
    };
  }
  // SAFETY: the null-prototype map starts empty; only validated entries are inserted below.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const projectedState: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
  const suppliedState = record(implementation.state);
  const stateEntries =
    current === undefined
      ? Object.entries(suppliedState)
      : Object.keys(suppliedState)
          .filter((name) => members[name]?.kind === "state")
          .map((name) => [name, suppliedState[name]] as const);
  for (const [name, valueValueNext] of stateEntries) {
    output.exposeState(name, source(valueValueNext));
    projectedState[name] = valueValueNext;
  }
  // SAFETY: the null-prototype map starts empty; only validated entries are inserted below.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const projectedOpaque: Record<string, unknown> = Object.create(null) as Record<string, unknown>;
  const suppliedOpaque = record(implementation.opaque ?? {});
  const opaqueEntries =
    current === undefined
      ? Object.entries(suppliedOpaque)
      : Object.keys(suppliedOpaque)
          .filter((name) => members[name]?.kind === "opaque")
          .map((name) => [name, suppliedOpaque[name]] as const);
  for (const [name, valueValue] of opaqueEntries) {
    output.exposeOpaque(name, opaqueCapability(members[name], valueValue));
    projectedOpaque[name] = valueValue;
  }
  const projection = freezeObject({
    methods: freezeObject(projectedMethods),
    state: freezeObject(projectedState),
    opaque: freezeObject(projectedOpaque),
    ...(variant === undefined ? {} : { variant }),
  });
  return {
    methods,
    projection,
    ...(Object.keys(aliases).length ? { aliases } : {}),
    ...(variant === undefined ? {} : { variant }),
  };
}
function setupShim(
  facet: FacetDefinition,
): (scope: RawScope, declaration: RawFacetRecorder) => undefined {
  return (scope, raw) => {
    const declaration: FacetDeclaration = freezeObject({
      serve<C extends AnyContract>(
        contract: C,
        factory: ObjectFactory<NoInfer<C>>,
        options: ServeOptions<NoInfer<C>> = {},
      ) {
        const prepared = prepareServeOptions(contract, options);
        if (prepared.adapters.length && raw.adapterVersion?.() !== 1) {
          throw executorFailure(
            "adapter_version",
            "WASM executor does not support legacy adapters",
          );
        }
        const adapters: RawAdapter[] = prepared.adapters.map((adapter) => ({
          provider: adapter.provider,
          consumer: adapter.consumer,
          adapt: (current, output) =>
            implementationOutput(
              // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
              // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
              JSON.parse(adapter.consumer) as Descriptor,
              invoke(adapter.adapt, [current]),
              output,
              current,
            ),
        }));
        raw.serve(
          canonicalJson(contract.descriptor),
          (input, principal, rawSelf) =>
            implementationOutput(
              contract.descriptor,
              invoke(factory, [detached(input), wrapObject(rawSelf, principal)]),
              rawSelf,
            ),
          prepared.flags,
          adapters,
        );
      },
    });
    voidResult(invoke(facet.setup, [wrapScope(scope), declaration], facet));
  };
}

/**
 * Executes trusted facet code against a BrowserClient's existing Rust Host. Construct it with the
 * initialized module for that same Host. The caller closes the client, lets outstanding callbacks
 * settle, then frees executor/client raw wrappers; a new executor does not reset the realm budget.
 * Code loading is not a sandbox: evaluation and callbacks can perform irreversible external effects.
 */
export class BrowserExecutor {
  /** Low-level owned executor wrapper; diagnostics track physical work separately from logical cancellation. */
  readonly rust: WasmExecutorPort;
  constructor(client: BrowserClient, module: ExecutorWasmModule) {
    this.rust = module.createCheckedWasmExecutor(client.rust);
  }
  /**
   * Installs trusted, already evaluated code and awaits eager Host-lifetime activation. This makes
   * no source/hash/closed-import claim. Initial installation is not a reload transaction: setup or
   * activation failure may leave earlier declarations/effects, so inspect the original rejection
   * and close the Host if the partial initial state is unusable.
   */
  async install(facet: FacetDefinition): Promise<void> {
    await this.rust.install(
      facet.name,
      canonicalJson(facet.uses.map((token) => token.descriptor)),
      setupShim(facet),
    );
  }
  /**
   * Copies and validates a prebuilt artifact, evaluates it, then installs selected (default "default").
   * Metadata, hash, closed imports and export checks precede evaluation; the artifact limit is
   * 1 MiB of UTF-8. After admission, failed evaluation/installation still consumes budget and may
   * retain module effects. Each document realm permits 32 evaluations and 8 MiB total source;
   * rotation_required requires a new realm, not another Host. Blob URL cleanup does not evict ESM.
   * Rejections preserve loader/core code and reason where supplied; this is trusted code execution.
   */
  async load(artifact: EsmArtifactV1, selected = "default"): Promise<void> {
    const { generation, definition, release } = await evaluateEsmArtifact(
      artifact,
      selected,
      (bytes) => this.rust.reserveGeneration(bytes),
    );
    try {
      const facet = record(definition);
      if (
        typeof facet.name !== "string" ||
        !Array.isArray(facet.uses) ||
        typeof facet.setup !== "function"
      ) {
        throw executorFailure("facet", "selected export is not a facet definition");
      }
      // SAFETY: the preceding name, uses and setup guards establish the executable facet shape.
      const typed = checkedFacet(facet);
      await generation.install(
        typed.name,
        canonicalJson(typed.uses.map((token) => token.descriptor)),
        setupShim(typed),
      );
    } finally {
      release();
    }
  }
  /**
   * Submits trusted executable definitions to one core reload transaction and awaits its report.
   * Successful commit advances the code version while compatible managed references survive.
   * A refused candidate does not become current, but prior-generation retirement and external
   * resource/callback effects are not universally rolled back. Dropping this Promise does not
   * request abort; an issued options.signal can request precommit abort. Keep inputs/options
   * unchanged until settlement. Overlapping admission is checked by the core rather than queued here.
   */
  async reload(
    facets: readonly FacetDefinition[],
    options: ReloadOptions = {},
  ): Promise<ReloadReport> {
    const candidates: RawCandidate[] = [];
    try {
      for (const facet of facets) {
        candidates.push(
          this.rust.prepare(
            facet.name,
            canonicalJson(facet.uses.map((token) => token.descriptor)),
            setupShim(facet),
          ),
        );
      }
      return await this.submit(candidates, options);
    } finally {
      for (const candidate of candidates) {
        candidate.free();
      }
    }
  }
  /**
   * Validates/evaluates artifacts sequentially, then submits their selected facets as one reload.
   * Each selected export defaults to "default". A later validation or core refusal can leave earlier
   * module evaluation effects and consumed realm budget; existing committed facets are not replaced
   * merely by evaluation. Keep the borrowed request collection/options unchanged until settlement.
   * Cancellation/deadlines follow reload and cannot establish physical completion or external rollback.
   *
   * @example
   * ```ts
   * import type { BrowserExecutor } from "@tembra/sdk/browser";
   * import type { EsmArtifactV1 } from "@tembra/sdk";
   * async function update(executor: BrowserExecutor, artifact: EsmArtifactV1): Promise<number> {
   *   const report = await executor.reloadArtifacts([{ artifact }], { gateDeadline: 2_000 });
   *   return report.codeVersion; // The core's committed version, not the artifact hash.
   * }
   * ```
   */
  async reloadArtifacts(
    artifacts: readonly { readonly artifact: EsmArtifactV1; readonly selected?: string }[],
    options: ReloadOptions = {},
  ): Promise<ReloadReport> {
    const candidates: RawCandidate[] = [];
    try {
      for (const { artifact, selected = "default" } of artifacts) {
        // oxlint-disable-next-line no-await-in-loop -- Each lifecycle observation depends on the previous step settling.
        const { generation, definition, release } = await evaluateEsmArtifact(
          artifact,
          selected,
          (bytes) => this.rust.reserveGeneration(bytes),
        );
        try {
          const facet = record(definition);
          if (
            typeof facet.name !== "string" ||
            !Array.isArray(facet.uses) ||
            typeof facet.setup !== "function"
          ) {
            throw executorFailure("facet", "selected export is not a facet definition");
          }
          // SAFETY: the preceding name, uses and setup guards establish the executable facet shape.
          const typed = checkedFacet(facet);
          candidates.push(
            generation.prepare(
              typed.name,
              canonicalJson(typed.uses.map((token) => token.descriptor)),
              setupShim(typed),
            ),
          );
        } finally {
          release();
        }
      }
      return await this.submit(candidates, options);
    } finally {
      for (const candidate of candidates) {
        candidate.free();
      }
    }
  }
  private async submit(
    candidates: readonly RawCandidate[],
    options: ReloadOptions,
  ): Promise<ReloadReport> {
    if (
      Object.keys(options).some(
        (key) =>
          !["remove", "gateDeadline", "retireDeadline", "taskStopDeadline", "signal"].includes(key),
      )
    ) {
      throw executorFailure("reload_options", "unknown reload option");
    }
    const { signal, ...flags } = options;
    const builder = this.rust.beginReload(canonicalJson(flags));
    let result: Promise<string>;
    try {
      for (const candidate of candidates) {
        builder.add(candidate);
      }
      if (signal !== undefined) {
        const raw = signals.get(signal);
        if (!raw) {
          throw executorFailure("reload_signal", "signal was not issued by this binding");
        }
        builder.signal(raw);
      }
      result = builder.start();
    } finally {
      builder.free();
    }
    const report = detached<
      Omit<ReloadReport, "previousVersion"> & { previousVersion: number | null }
    >(await result);
    return freezeObject({ ...report, previousVersion: report.previousVersion ?? undefined });
  }
  /**
   * Returns a detached frozen observation of Rust jobs/queues/generations and realm admissions.
   * Counters grant no mutation authority; jobs may remain after logical cancellation until physical
   * callbacks settle. Use them to observe cleanup, not to infer that external side effects rolled back.
   */
  diagnostics(): Readonly<Record<string, number>> & {
    readonly jobs: number;
    readonly callbacks: number;
    readonly queued: number;
    readonly liveGenerations: number;
  } {
    return freezeObject({
      // SAFETY: the Rust ABI serializes this declared shape after validating the bound contract.
      // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
      ...(JSON.parse(this.rust.diagnosticsJson()) as Record<string, number> & {
        jobs: number;
        callbacks: number;
        queued: number;
        liveGenerations: number;
      }),
      ...realmDiagnostics(),
    });
  }
}
