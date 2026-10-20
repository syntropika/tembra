// oxlint-disable max-classes-per-file -- The issued binding port and public client share this private ABI translation boundary.
import { bindReference, canonicalJson, ContractError } from "../index.js";
import type {
  AnyContract,
  BindingPort,
  CallRequest,
  ContextHandle,
  InputOf,
  Payload,
  Readable,
  ReferenceOf,
  RefStatus,
  Shape,
} from "../index.js";
import type { WasmMethod } from "./abi.js";

// oxlint-disable-next-line oxc/no-barrel-file -- This package entrypoint preserves the public executor API while implementation modules stay separate.
export * from "./executor.js";
export { inspectEsmSource, validateEsmArtifact, realmDiagnostics } from "./artifact.js";

/**
 * Low-level wasm-bindgen subscription. Unsubscribe before freeing its wrapper; normal SDK
 * subscriptions return a cleanup function that performs both once. Dispatch remains in Rust.
 */
export type WasmSubscription = { unsubscribe: WasmMethod<[], void>; free: WasmMethod<[], void> };
/** Low-level issued readable; snapshot/event strings are Rust-validated JSON, not mutable shared state. */
export type WasmReadable = {
  snapshotJson: WasmMethod<[], string>;
  awaitHydrated: WasmMethod<[], Promise<string>>;
  subscribe: WasmMethod<[callback: (snapshotJson: string) => void], WasmSubscription>;
  subscribeEvents: WasmMethod<[callback: (eventJson: string) => void], WasmSubscription>;
  free: WasmMethod<[], void>;
};
/** Low-level issued reference: release ends consumer ownership; free releases the WASM wrapper. */
export type WasmReference = {
  statusJson: WasmMethod<[], string>;
  shapeJson: WasmMethod<[], string>;
  errorJson: WasmMethod<[], string>;
  ready: WasmMethod<[], Promise<void>>;
  call: WasmMethod<[member: string, payloadJson: string, context: unknown], Promise<string>>;
  readable: WasmMethod<[member: string], WasmReadable>;
  onStatus: WasmMethod<[callback: (statusJson: string) => void], WasmSubscription>;
  onShape: WasmMethod<[callback: (shapeJson: string) => void], WasmSubscription>;
  release: WasmMethod<[], void>;
  free: WasmMethod<[], void>;
};
/**
 * Owned websocket connection. Await ready for its handshake; closed resolves with a reason after
 * shutdown. close requests shutdown and is safe to repeat; free the wrapper after closure.
 * A disconnect does not establish whether an already dispatched remote operation committed.
 */
export type WasmConnection = {
  ready: WasmMethod<[], Promise<void>>;
  closed: WasmMethod<[], Promise<string>>;
  close: WasmMethod<[], void>;
  free: WasmMethod<[], void>;
  status: WasmMethod<[], string>;
};
/** Raw Host ABI supplied by the instantiated module; close it before freeing its wrapper. */
export type WasmHost = {
  hold: WasmMethod<[descriptor: string, input: string, options: string], WasmReference>;
  connect: WasmMethod<[url: string, provider: string], WasmConnection>;
  runRoot: WasmMethod<[callback: (token: object) => Promise<unknown>], Promise<unknown>>;
  cancelContext: WasmMethod<[token: unknown], void>;
  close: WasmMethod<[], Promise<void>>;
  errorsJson: WasmMethod<[], string>;
  free: WasmMethod<[], void>;
};
/** Core Host construction policy; invalid values are refused by the WASM constructor. */
export type BrowserHostOptions = {
  /** Optional bounded idle consumer cache; it neither retains released JS references nor changes provider lifetime. */
  readonly consumerLinger?: {
    /** Contract-name TTLs in whole milliseconds; omitted names retain no idle binding. */
    readonly types: Readonly<Record<string, number>>;
    /** Defaults to 4; integer range 0..1,024. Zero disables idle retention, as does a zero TTL. */
    readonly capacity?: number;
  };
};
/** Already initialized wasm-bindgen module; this package does not download or instantiate WASM. */
export type WasmModule = {
  WasmHost: {
    new (name: string): WasmHost;
    withOptions?: WasmMethod<[name: string, options: string], WasmHost>;
  };
};

type Envelope<T> = { present: false } | { present: true; value: T };
// oxlint-disable-next-line typescript/no-unnecessary-type-parameters -- The issued Rust accessor determines the serialized envelope type at each call site.
function absent<T>(raw: string): T | undefined {
  // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
  const result = JSON.parse(raw) as Envelope<T>;
  return result.present ? result.value : undefined;
}
function shape(value: Shape & { variant: string | null }): Shape {
  return { ...value, variant: value.variant ?? undefined };
}
function remove(subscription: WasmSubscription): () => void {
  let active = true;
  return () => {
    if (active) {
      active = false;
      subscription.unsubscribe();
      subscription.free();
    }
  };
}

/**
 * Maps SDK operations to an issued Rust reference; returned JSON values are detached copies.
 * Intended for embedding adapters. Normal callers use BrowserClient.hold, which constructs this
 * port and checks contract state/results. The reference's backend remains its lifecycle owner.
 */
export class WasmBindingPort implements BindingPort {
  readonly #reference: WasmReference;
  readonly #contract: AnyContract;
  readonly #readables = new Map<string, Readable<unknown>>();
  constructor(contract: AnyContract, reference: WasmReference) {
    this.#contract = contract;
    this.#reference = reference;
  }
  get status(): RefStatus {
    // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
    return JSON.parse(this.#reference.statusJson()) as RefStatus;
  }
  get shape(): Shape | undefined {
    const current = absent<Shape & { variant: string | null }>(this.#reference.shapeJson());
    return current === undefined ? undefined : shape(current);
  }
  get variant(): string | undefined {
    return this.shape?.variant;
  }
  get error(): unknown {
    return absent(this.#reference.errorJson());
  }
  get ready(): Promise<void> {
    return this.#reference.ready();
  }
  async call(request: CallRequest): Promise<Payload> {
    if (request.contract.fingerprint !== this.#contract.descriptor.fingerprint) {
      throw new ContractError("contract", "call contract differs from the bound consumer");
    }
    if (
      request.variant !== undefined &&
      this.variant !== undefined &&
      request.variant !== this.variant
    ) {
      throw new ContractError("not_in_variant", "call variant differs from the current shape");
    }
    // Check before serialization can erase NaN, undefined fields or invalid JS objects.
    const payload = canonicalJson(request.args);
    // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
    const result = await this.#reference.call(request.member, payload, request.context?.handle);
    // SAFETY: the issued Rust reference validates and serializes its portable response against the bound consumer.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked ABI supplies the payload envelope.
    return JSON.parse(result) as Payload;
  }
  readable(member: string): Readable<unknown> {
    let readable = this.#readables.get(member);
    if (!readable) {
      const source = this.#reference.readable(member);
      readable = {
        get value(): unknown {
          // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
          // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
          return (JSON.parse(source.snapshotJson()) as { value: unknown }).value;
        },
        subscribe: (listener) =>
          remove(
            source.subscribe((raw) => {
              // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
              // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
              listener((JSON.parse(raw) as { value: unknown }).value);
            }),
          ),
      };
      this.#readables.set(member, readable);
    }
    return readable;
  }
  opaque(member: string): never {
    throw new ContractError(
      "native",
      `remote reference ${this.#contract.descriptor.name}.${member} has no native opaque value`,
    );
  }
  release(): void {
    this.#reference.release();
  }
  onStatus(listener: (status: RefStatus, error: unknown) => void): () => void {
    return remove(
      this.#reference.onStatus((raw) => {
        // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
        const event = JSON.parse(raw) as { status: RefStatus; error: unknown };
        listener(event.status, event.error);
      }),
    );
  }
  onShape(listener: (value: Shape) => void): () => void {
    return remove(
      this.#reference.onShape((raw) => {
        // SAFETY: this Rust-issued handle serializes the declared payload shape for its checked contract.
        // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The checked boundary preserves the type described above.
        listener(shape(JSON.parse(raw) as Shape & { variant: string | null }));
      }),
    );
  }
}

/**
 * Browser consumer facade for an already initialized WASM module. The Rust Host owns dispatch,
 * identity, hydration and object lifetime. Await connection/reference readiness before use;
 * release references and await close before freeing the raw Host wrapper.
 *
 * @example
 * ```ts
 * import { BrowserClient } from "@tembra/sdk/browser";
 * import type { WasmModule } from "@tembra/sdk/browser";
 * import { hostNamed, method, remote, Type } from "@tembra/sdk";
 * const Counter = remote("counter", {
 *   version: 1, input: Type.Object({}), host: hostNamed("provider"),
 *   variants: { live: { methods: { read: method(undefined, Type.Integer()) } } },
 * });
 * async function readCounter(module: WasmModule, url: string): Promise<number> {
 *   const client = new BrowserClient(module);
 *   const connection = client.connect(url, "provider");
 *   const counter = client.hold(Counter, {});
 *   try {
 *     await connection.ready();
 *     await counter.ready;
 *     return await counter.read();
 *   } finally {
 *     counter.release();
 *     connection.close();
 *     await client.close();
 *     connection.free();
 *     client.rust.free();
 *   }
 * }
 * ```
 */
export class BrowserClient {
  /** Raw owned Host wrapper; free only after awaited close and any embedding-owned work settles. */
  readonly rust: WasmHost;
  /** Name defaults to "browser". Passing options requires the module's withOptions constructor. */
  constructor(module: WasmModule, name = "browser", options?: BrowserHostOptions) {
    if (options === undefined) {
      this.rust = new module.WasmHost(name);
    } else {
      if (!module.WasmHost.withOptions) {
        throw new ContractError("wasm_options", "WASM module does not expose constructor options");
      }
      this.rust = module.WasmHost.withOptions(name, canonicalJson(options));
    }
  }
  /**
   * Starts a websocket connection to the named provider. No automatic reconnect is installed.
   * Retain the returned handle, await ready(), and close/free it when finished.
   */
  connect(url: string, provider: string): WasmConnection {
    return this.rust.connect(url, provider);
  }
  /**
   * Acquires a retained consumer reference immediately, possibly still loading. Await ready;
   * state defaults to true (initial hydration); false requests shape-only readiness. Calls may
   * reject with structured core/transport errors. Release the reference when finished, including
   * after failed readiness. Synchronous serialization failures occur before Rust acquisition.
   */
  hold<C extends AnyContract>(
    contract: C,
    input: InputOf<C>,
    options: { state?: boolean } = {},
  ): ReferenceOf<C> {
    const reference = this.rust.hold(
      canonicalJson(contract.descriptor),
      canonicalJson(input),
      canonicalJson(options),
    );
    return bindReference(contract, new WasmBindingPort(contract, reference));
  }
  /**
   * Runs and awaits an operation under a fresh issued caller Context. The callback's settled
   * result is preserved; the Context must not be reused as an independent root after it expires.
   * A rejection does not undo effects already performed by the callback or a remote provider.
   */
  async root<T>(operation: (context: ContextHandle) => Promise<T>): Promise<T> {
    const result = await this.rust.runRoot(
      async (token) => await operation(Object.freeze({ runtime: "tembra", handle: token })),
    );
    // SAFETY: the Rust root returns the exact settled value of this submitted T callback without conversion.
    // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- The issued callback establishes the result type.
    return result as T;
  }

  /** Requests cancellation of this issued scope; it does not terminate JavaScript or undo committed work. */
  cancel(context: ContextHandle): void {
    this.rust.cancelContext(context.handle);
  }
  /**
   * Cancels issued roots, stops new acquisitions and awaits core-owned invocations/resource cleanup.
   * Repeated calls observe the same core shutdown. A callback that never settles can keep it pending;
   * cleanup failures reject with the structured unavailable/cleanup aggregate after attempted cleanup.
   * Earlier effects are not rolled back. Raw WASM wrappers still require free; arbitrary external
   * JavaScript work remains cooperative.
   */
  async close(): Promise<void> {
    await this.rust.close();
  }
}
