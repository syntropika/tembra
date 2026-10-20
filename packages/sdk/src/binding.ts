import type { Static, TSchema } from "typebox";

import type {
  AnyContract,
  ContractToken,
  Descriptor,
  GeneratedContract,
  Infer,
  LocalContract,
  Method,
  NativeSchema,
  Opaque,
  RemoteContract,
  VariantDefinition,
} from "./contracts.js";
import { ContractError } from "./error.js";
import { assertValid, cleanArguments, object } from "./schema.js";

const defineErrorProperties = Object.defineProperties;
const stringifyError = JSON.stringify;
const additionalErrorKeys = Symbol.for("errorAdditionalPropertyKeys");
function nativeContextRequired(): ContractError {
  const message = "native signature calls require an explicit managed Context";
  const error = new ContractError("native_context", message);
  // This is a typed failure representation for both foreign adapters, never authority.
  defineErrorProperties(error, {
    code: { value: "invalid", enumerable: true },
    detailsJson: { value: "null", enumerable: true },
    tembraRuntimeError: {
      value: stringifyError({
        code: "invalid",
        reason: "native_context",
        message: error.message,
        details: null,
      }),
      enumerable: true,
    },
    [additionalErrorKeys]: { value: ["tembraRuntimeError"] },
  });
  return error;
}

/** Backend-issued caller scope. Copying this shape does not create a valid managed Context. */
export type ContextHandle = {
  readonly runtime: "tembra";
  readonly handle: unknown;
};
/** Explicit payload presence: absent and a present JSON null are different protocol values. */
export type Payload =
  | { readonly present: false }
  | { readonly present: true; readonly value: unknown };
/** Loading precedes readiness; released is caller disposal, while gone is provider loss. */
export type RefStatus = "loading" | "live" | "released" | "gone";
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/** A current value with synchronous notifications; retain and call the returned unsubscribe. */
export type Readable<T> = {
  readonly value: T;
  subscribe(listener: (value: T) => void): () => void;
};
/* oxlint-enable typescript/method-signature-style */
/** Current provider view; remote variants can change which members are available. */
export type Shape = {
  readonly variant: string | undefined;
  readonly members: readonly string[];
  readonly contract: string;
};
/** Adapter request after contract lookup; a native channel requires an issued caller Context. */
export type CallRequest = {
  readonly contract: Descriptor;
  readonly variant: string | undefined;
  readonly member: string;
  readonly args: Payload;
  readonly context?: ContextHandle;
};

/** Implemented by Rust/WASM or native executor adapters, never a second object runtime. */
/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
export type BindingPort = {
  readonly status: RefStatus;
  readonly variant: string | undefined;
  readonly shape: Shape | undefined;
  readonly error: unknown;
  readonly ready: Promise<void>;
  call(request: CallRequest): Promise<Payload>;
  readable(member: string): Readable<unknown>;
  opaque(member: string): unknown;
  release(): void;
  onStatus(listener: (status: RefStatus, error: unknown) => void): () => void;
  onShape(listener: (shape: Shape) => void): () => void;
};
/* oxlint-enable typescript/method-signature-style */

/* oxlint-disable typescript/method-signature-style -- public methods preserve their existing parameter variance */
/**
 * Retained consumer binding. Await ready before relying on its shape or hydrated state.
 * The owner releases it when finished and removes subscriptions it created. Backend errors
 * remain available through error/onStatus; Promise rejection values are not limited to ContractError.
 */
export type ContractReference = {
  readonly contract: ContractToken;
  readonly status: RefStatus;
  readonly variant: string | undefined;
  readonly shape: Shape | undefined;
  readonly error: unknown;
  /** Resolves after backend readiness; rejects if acquisition fails before readiness. */
  readonly ready: Promise<void>;
  /** Releases this binding through its issuing backend; retained member aliases gain no new lifetime. */
  release(): void;
  onStatus(listener: (status: RefStatus, error: unknown) => void): () => void;
  onShape(listener: (shape: Shape) => void): () => void;
};
/* oxlint-enable typescript/method-signature-style */

type HasNative<S> = S extends { readonly native: string } ? true : false;
type CallContext<A, R> = true extends HasNative<A> | HasNative<R>
  ? [context: ContextHandle]
  : [context?: ContextHandle];
/** Foreign native signatures require an exact managed caller scope for result ownership. */
type Call<A, R> = [A] extends [undefined]
  ? (...context: CallContext<A, R>) => Promise<[R] extends [undefined] ? undefined : Infer<R>>
  : (
      args: Infer<A>,
      ...context: CallContext<A, R>
    ) => Promise<[R] extends [undefined] ? undefined : Infer<R>>;
type MemberValue<S> =
  S extends Method<infer A, infer R>
    ? Call<A, R>
    : S extends Opaque<infer T>
      ? T
      : S extends TSchema
        ? Readable<Static<S>>
        : never;
type LocalView<M> = { readonly [K in keyof M]: MemberValue<M[K]> };
type VariantView<V extends VariantDefinition> = {
  readonly [K in keyof V["state"]]: V["state"][K] extends TSchema
    ? Readable<Static<V["state"][K]>>
    : never;
} & {
  readonly [K in keyof V["methods"]]: V["methods"][K] extends Method<infer A, infer R>
    ? Call<A, R>
    : never;
};
/** Contract-specific state readables and async methods, together with consumer lifecycle operations. */
export type ReferenceOf<C> =
  C extends GeneratedContract<unknown, infer R>
    ? R
    : C extends LocalContract<TSchema, infer M>
      ? ContractReference & LocalView<M>
      : C extends RemoteContract<TSchema, infer V>
        ? {
            [K in keyof V]: ContractReference & {
              readonly variant: K | undefined;
            } & VariantView<V[K]>;
          }[keyof V]
        : ContractReference;

function isNative(value: unknown): value is NativeSchema {
  return value !== null && typeof value === "object" && Object.hasOwn(value, "native");
}

function assertContextHandle(context: unknown): asserts context is ContextHandle {
  const data = object(context, "foreign_ctx");
  if (data.runtime !== "tembra" || !Object.hasOwn(data, "handle")) {
    throw new ContractError("foreign_ctx", "invalid context handle");
  }
}

/**
 * Builds a typed consumer view over an existing backend-owned binding; it does not acquire an object.
 * Stable method/readable wrappers recheck the current variant on use. Portable arguments are
 * cleaned and validated before dispatch; results are validated after execution, so a result
 * validation failure does not undo provider effects. Native signatures require an explicit Context.
 * Prefer the embedding's hold/get operation unless implementing a binding adapter.
 */
export function bindReference<C extends AnyContract>(
  contract: C,
  port: BindingPort,
): ReferenceOf<C> {
  const callers = new Map<string, (...args: unknown[]) => Promise<unknown>>();
  const readables = new Map<string, Readable<unknown>>();
  const known = (name: string) =>
    contract.descriptor.variants[port.variant ?? ""]?.[name] ??
    (port.status === "loading"
      ? Object.values(contract.descriptor.variants)
          .map((members) => members[name])
          .find((value) => value !== undefined)
      : undefined);
  const base: ContractReference = {
    contract,
    get status() {
      return port.status;
    },
    get variant() {
      return port.variant;
    },
    get shape() {
      return port.shape;
    },
    get error() {
      return port.error;
    },
    get ready() {
      return port.ready;
    },
    release: () => {
      port.release();
    },
    onStatus: (listener) => port.onStatus(listener),
    onShape: (listener) => port.onShape(listener),
  };
  const reference = new Proxy(base, {
    get(target, property, receiver) {
      if (property === "then") {
        return undefined;
      }
      if (Object.hasOwn(target, property)) {
        const value: unknown = Reflect.get(target, property, receiver);
        return value;
      }
      if (typeof property !== "string") {
        return undefined;
      }
      const member = known(property);
      if (member?.kind === "state") {
        let readable = readables.get(property);
        if (!readable) {
          const source = port.readable(property);
          const checked = (value: unknown) => {
            const selected = known(property);
            if (selected?.kind !== "state") {
              throw new ContractError("not_in_variant", `state ${property} is unavailable`);
            }
            assertValid(selected.schema, value);
            return value;
          };
          readable = {
            get value() {
              return checked(source.value);
            },
            subscribe(listener) {
              // Preserve the runtime result so the backend can refuse asynchronous listeners.
              // oxlint-disable-next-line typescript/no-confusing-void-expression -- discarding a Promise would bypass the backend's synchronous callback guard.
              return source.subscribe((value) => listener(checked(value)));
            },
          };
          readables.set(property, readable);
        }
        return readable;
      }
      if (member?.kind === "opaque") {
        return port.opaque(property);
      }
      if (member?.kind !== "method") {
        return undefined;
      }
      let caller = callers.get(property);
      if (!caller) {
        caller = async (...given: unknown[]) => {
          const selected = known(property);
          if (selected?.kind !== "method") {
            throw new ContractError("not_in_variant", `method ${property} is unavailable`);
          }
          const takesArgs = selected.args !== undefined;
          if (given.length > (takesArgs ? 2 : 1) || (takesArgs && !given.length)) {
            throw new ContractError("args", "incorrect method argument count");
          }
          const context = given[takesArgs ? 1 : 0];
          if ((isNative(selected.args) || isNative(selected.result)) && context === undefined) {
            throw nativeContextRequired();
          }
          if (context !== undefined) {
            assertContextHandle(context);
          }
          let args: Payload = { present: false };
          if (takesArgs) {
            const value = isNative(selected.args)
              ? given[0]
              : cleanArguments(selected.args, given[0]);
            if (!isNative(selected.args)) {
              assertValid(selected.args, value);
            }
            args = { present: true, value };
          }
          const result = await port.call({
            contract: contract.descriptor,
            variant: port.variant,
            member: property,
            args,
            ...(context === undefined ? {} : { context }),
          });
          if (selected.result === undefined) {
            return undefined;
          }
          if (!result.present) {
            throw new ContractError("result", "provider omitted a required result");
          }
          if (!isNative(selected.result)) {
            assertValid(selected.result, result.value);
          }
          return result.value;
        };
        callers.set(property, caller);
      }
      return caller;
    },
  });
  // SAFETY: descriptor-selected proxy members validate portable arguments/results and expose only the typed local port's native values.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- TypeScript cannot express this descriptor-selected generic proxy projection
  return reference as ReferenceOf<C>;
}
