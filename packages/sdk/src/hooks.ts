import type { TArray, TSchema } from "typebox";

import type { Infer, Method, NativeSchema, ValueDescriptor } from "./contracts.js";
import { native } from "./contracts.js";
import { ContractError } from "./error.js";
import type {
  Completion,
  ManagedContext,
  ObjectDeclaration,
  Registry,
  RegisteredOperation,
} from "./executor.js";
import { canonicalJson, clonePortable, fingerprint } from "./portable.js";
import { normalizeSchema, object } from "./schema.js";

/** Ordered series, first-stop bail, value-transforming waterfall, scoped middleware, or parallel fan-out. */
export type HookStrategy = "series" | "bail" | "waterfall" | "middleware" | "parallel";
/** Hook payload schema, local native identity or undefined for an absent payload position. */
export type HookValue = TSchema | NativeSchema | undefined;
/** Payload positions of the terminal operation invoked after the hook's contributions. */
export type HookTerminalProfile<
  I extends HookValue = HookValue,
  R extends HookValue = HookValue,
> = {
  readonly input: I;
  readonly result: R;
};
/** Canonical hook metadata, including null for absent payload positions and its verified fingerprint. */
export type HookDescriptor = {
  readonly format: 1;
  readonly name: string;
  readonly version: number;
  readonly strategy: HookStrategy;
  readonly input: ValueDescriptor | null;
  readonly result: ValueDescriptor | null;
  readonly terminal: null | {
    readonly input: ValueDescriptor | null;
    readonly result: ValueDescriptor | null;
  };
  readonly fingerprint: string;
};
/** Continue to the next contribution, or stop the bail strategy with this result value. */
export type BailDecision<R> =
  | { readonly kind: "continue" }
  | { readonly kind: "stop"; readonly value: R };
/** Callable job capacity errors expose `RuntimeError.details.callablePhase` across backends.
 * `ingress` means refusal before the callable body: this request did not reserve Next and
 * may retry after capacity is available. `admitted` means the body was entered; a primary
 * Next failure requires explicit `recover`, while a secondary misuse remains independent.
 * Phase is not a generic consumed-attempt flag and other error reasons need their own handling.
 * Accepted input conversion is deferred: a mutable object is read when its decoder runs,
 * rather than copied at this call boundary. */
export type Next<A, R> = {
  (input: A): Promise<R>;
  readonly recover: (fallback: R) => Promise<R>;
};
/** Middleware input and its single-use continuation; recover explicitly after an admitted failure. */
export type MiddlewareArgs<A, R> = {
  readonly input: A;
  readonly next: Next<A, R>;
};
/** Input view for strategies without a middleware continuation. */
export type PlainHookArgs<A> = { readonly input: A };
type Arguments<S, I, R> = S extends "middleware"
  ? MiddlewareArgs<Infer<I>, Infer<R>>
  : PlainHookArgs<Infer<I>>;
type Output<S, I, R> = S extends "series"
  ? undefined
  : S extends "bail"
    ? BailDecision<Infer<R>>
    : S extends "waterfall"
      ? Infer<I>
      : Infer<R>;
type OutputSchema<S, I, R> = S extends "series"
  ? undefined
  : S extends "bail"
    ? NativeSchema<BailDecision<Infer<R>>>
    : S extends "waterfall"
      ? I
      : R;

/** Frozen hook metadata and registry signature; constructing it does not install contributions. */
export type HookDefinition<
  S extends HookStrategy = HookStrategy,
  I extends HookValue = HookValue,
  R extends HookValue = HookValue,
  T extends HookTerminalProfile | undefined = HookTerminalProfile | undefined,
> = {
  readonly descriptor: HookDescriptor & { readonly strategy: S };
  readonly canonical: string;
  readonly registry: Registry;
  readonly input: I;
  readonly result: R;
  readonly terminal: T;
  readonly method: Method<NativeSchema<Arguments<S, I, R>>, OutputSchema<S, I, R>>;
};
/** Common verified hook surface for registries that accept several strategies. */
export type AnyHookDefinition = {
  readonly descriptor: HookDescriptor;
  readonly canonical: string;
  readonly registry: Registry;
  readonly input: HookValue;
  readonly result: HookValue;
  readonly terminal: HookTerminalProfile | undefined;
  readonly method: { readonly kind: "method"; readonly args?: unknown; readonly result?: unknown };
};
/** Payload admitted to this hook; an omitted input position carries undefined. */
export type HookInput<H extends AnyHookDefinition> = Infer<H["input"]>;
/** Handler result derived from the strategy; series returns undefined and bail returns continue/stop. */
export type HookOutput<H extends AnyHookDefinition> = Output<
  H["descriptor"]["strategy"],
  H["input"],
  H["result"]
>;
/**
 * Managed handler for one hook invocation. Its arguments follow the declared strategy, and its
 * completion is awaited before the invocation settles. Check context cancellation cooperatively;
 * rejection cannot undo effects already performed by the handler.
 */
export type HookHandler<H extends AnyHookDefinition> = (
  context: ManagedContext,
  args: Arguments<H["descriptor"]["strategy"], H["input"], H["result"]>,
) => Completion<HookOutput<H>>;

type InputOption<I> = [I] extends [undefined] ? { readonly input?: I } : { readonly input: I };
type ResultOption<R> = [R] extends [undefined] ? { readonly result?: R } : { readonly result: R };
type Base<I> = { readonly version: number } & InputOption<I>;
type ParallelVector<R extends HookValue> = R extends TSchema
  ? TArray<R> | NativeSchema<readonly Infer<R>[]>
  : NativeSchema<readonly Infer<R>[]>;
type VectorTerminal<R extends HookValue, O extends HookValue = HookValue> = {
  readonly input: ParallelVector<R>;
  readonly result: O;
};

/**
 * Validates and freezes a hook signature without invoking handlers. Omitted payload positions
 * mean absence, not JSON null. Middleware requires a terminal profile; waterfall preserves its
 * input schema, and parallel terminals consume the result vector. Invalid combinations throw
 * ContractError before returning a definition.
 */
export function defineHook<const T extends HookTerminalProfile<undefined> | undefined = undefined>(
  name: string,
  options: {
    readonly version: number;
    readonly strategy: "series";
    readonly input?: undefined;
    readonly result?: never;
    readonly terminal?: T;
  },
): HookDefinition<"series", undefined, undefined, T>;
export function defineHook<
  const R extends HookValue = undefined,
  const T extends HookTerminalProfile<undefined, NoInfer<R>> | undefined = undefined,
>(
  name: string,
  options: {
    readonly version: number;
    readonly strategy: "bail";
    readonly input?: undefined;
    readonly terminal?: T;
  } & ResultOption<R>,
): HookDefinition<"bail", undefined, R, T>;
export function defineHook<
  const I extends HookValue = undefined,
  const T extends HookTerminalProfile<NoInfer<I>> | undefined = undefined,
>(
  name: string,
  options: Base<I> & {
    readonly strategy: "series";
    readonly result?: never;
    readonly terminal?: T;
  },
): HookDefinition<"series", I, undefined, T>;
export function defineHook<
  const I extends HookValue = undefined,
  const R extends HookValue = undefined,
  const T extends HookTerminalProfile<NoInfer<I>, NoInfer<R>> | undefined = undefined,
>(
  name: string,
  options: Base<I> &
    ResultOption<R> & {
      readonly strategy: "bail";
      readonly terminal?: T;
    },
): HookDefinition<"bail", I, R, T>;
export function defineHook<
  const I extends HookValue = undefined,
  const T extends HookTerminalProfile<NoInfer<I>> | undefined = undefined,
>(
  name: string,
  options: Base<I> & {
    readonly strategy: "waterfall";
    readonly result?: never;
    readonly terminal?: T;
  },
): HookDefinition<"waterfall", I, I, T>;
export function defineHook<
  const I extends HookValue = undefined,
  const R extends HookValue = undefined,
>(
  name: string,
  options: Base<I> &
    ResultOption<R> & {
      readonly strategy: "middleware";
      readonly terminal: HookTerminalProfile<NoInfer<I>, NoInfer<R>>;
    },
): HookDefinition<"middleware", I, R, HookTerminalProfile<I, R>>;
export function defineHook<
  const I extends HookValue = undefined,
  const R extends HookValue = undefined,
  const T extends VectorTerminal<NoInfer<R>> | undefined = undefined,
>(
  name: string,
  options: Base<I> & ResultOption<R> & { readonly strategy: "parallel"; readonly terminal?: T },
): HookDefinition<"parallel", I, R, T>;
/** Compiles immutable hook metadata. It performs no registration, setup, or hook execution. */
export function defineHook(name: string, options: unknown): AnyHookDefinition {
  const source = object(options, "hook_definition");
  fields(source, ["version", "strategy", "input", "result", "terminal"], "hook_definition");
  const { strategy } = source;
  const input = position(source.input);
  const result = strategy === "waterfall" ? input : position(source.result);
  if ((strategy === "series" || strategy === "waterfall") && source.result !== undefined) {
    fail("hook_strategy", "this strategy derives its handler result");
  }
  const terminal = source.terminal === undefined ? null : terminalProfile(source.terminal, false);
  return materializeHook(
    check({ format: 1, name, version: source.version, strategy, input, result, terminal }, false),
  );
}

/** Verifies peer metadata and its digest. Application value types remain unknown. */
/** Copies and verifies untrusted canonical hook metadata and fingerprint; throws ContractError on refusal. */
export function hookFromDescriptor(value: unknown): AnyHookDefinition {
  return materializeHook(check(clonePortable(value), true));
}

function fail(reason: string, message: string): never {
  throw new ContractError(reason, message);
}
function fields(value: Record<string, unknown>, allowed: readonly string[], reason: string): void {
  if (Object.keys(value).some((key) => !allowed.includes(key))) {
    fail(reason, "unknown field");
  }
}
function position(value: unknown): ValueDescriptor | null {
  if (value === undefined || value === null) {
    return null;
  }
  if (value !== null && typeof value === "object" && Object.hasOwn(value, "native")) {
    const record = object(value, "hook_definition");
    fields(record, ["native"], "hook_definition");
    if (typeof record.native !== "string") {
      fail("native_id", "nominal ID must be name@version");
    }
    return native(record.native);
  }
  return normalizeSchema(value);
}
function terminalProfile(
  value: unknown,
  peer: boolean,
): NonNullable<HookDescriptor["terminal"]> | null {
  if (value === null) {
    return null;
  }
  const record = object(value, "hook_definition");
  fields(record, ["input", "result"], "hook_definition");
  if (peer && (!Object.hasOwn(record, "input") || !Object.hasOwn(record, "result"))) {
    fail("hook_definition", "terminal positions are required");
  }
  return { input: position(record.input), result: position(record.result) };
}
function check(raw: unknown, peer: boolean): HookDescriptor {
  const source = object(raw, "hook_definition");
  fields(
    source,
    [
      "format",
      "name",
      "version",
      "strategy",
      "input",
      "result",
      "terminal",
      ...(peer ? ["fingerprint"] : []),
    ],
    "hook_definition",
  );
  if (
    ["format", "name", "version", "strategy", "input", "result", "terminal"].some(
      (key) => !Object.hasOwn(source, key),
    )
  ) {
    fail("hook_definition", "descriptor fields are required");
  }
  if (
    source.format !== 1 ||
    typeof source.name !== "string" ||
    !source.name ||
    /\p{Cc}/u.test(source.name)
  ) {
    fail("hook_definition", "invalid descriptor format or name");
  }
  if (
    typeof source.version !== "number" ||
    !Number.isSafeInteger(source.version) ||
    source.version < 1
  ) {
    fail("hook_definition", "version must be a positive safe integer");
  }
  if (!isStrategy(source.strategy)) {
    fail("hook_strategy", "unknown hook strategy");
  }
  const { strategy } = source;
  const input = position(source.input);
  const result = position(source.result);
  const terminal = terminalProfile(source.terminal, peer);
  validateTerminal(strategy, input, result, terminal);
  const body = {
    format: 1 as const,
    name: source.name,
    version: source.version,
    strategy,
    input,
    result,
    terminal,
  };
  const digest = fingerprint(body);
  if (
    peer &&
    (typeof source.fingerprint !== "string" ||
      !/^[a-f0-9]{64}$/u.test(source.fingerprint) ||
      source.fingerprint !== digest)
  ) {
    fail("hook_definition", "hook fingerprint mismatch");
  }
  return deepFreeze({ ...body, fingerprint: digest });
}
function isStrategy(value: unknown): value is HookStrategy {
  return (
    value === "series" ||
    value === "bail" ||
    value === "waterfall" ||
    value === "middleware" ||
    value === "parallel"
  );
}
function same(a: unknown, b: unknown): boolean {
  return canonicalJson(a) === canonicalJson(b);
}
function validateTerminal(
  strategy: HookStrategy,
  input: ValueDescriptor | null,
  result: ValueDescriptor | null,
  terminal: HookDescriptor["terminal"],
): void {
  if (strategy === "series" && result !== null) {
    fail("hook_definition", "series result must be unit");
  }
  if (strategy === "waterfall" && !same(input, result)) {
    fail("hook_definition", "waterfall result must match its input");
  }
  if (strategy === "middleware" && terminal === null) {
    fail("hook_definition", "middleware requires a terminal profile");
  }
  if (terminal !== null) {
    if (strategy !== "parallel" && !same(input, terminal.input)) {
      fail("hook_definition", "terminal input differs from hook input");
    }
    if ((strategy === "middleware" || strategy === "bail") && !same(result, terminal.result)) {
      fail("hook_definition", "terminal result differs from hook result");
    }
    if (strategy === "parallel") {
      const vector = terminal.input;
      if (vector === null) {
        fail("hook_definition", "parallel terminal requires a vector profile");
      }
      if (
        !isNative(vector) &&
        (result === null || isNative(result) || !same(vector, { type: "array", items: result }))
      ) {
        fail(
          "hook_definition",
          "parallel terminal requires an explicit native vector or portable array of its result",
        );
      }
    }
  }
}
function isNative(value: ValueDescriptor): value is { readonly native: string } {
  return typeof value === "object" && value !== null && Object.hasOwn(value, "native");
}
function deepFreeze<T>(value: T): T {
  if (value !== null && typeof value === "object") {
    for (const child of Object.values(value)) {
      deepFreeze(child);
    }
    Object.freeze(value);
  }
  return value;
}
function materializeHook(descriptor: HookDescriptor): AnyHookDefinition {
  const { fingerprint: digest, ...body } = descriptor;
  const result =
    descriptor.strategy === "bail"
      ? native(`tembra.hook.bail/${digest}@1`)
      : (descriptor.result ?? undefined);
  return deepFreeze({
    descriptor,
    canonical: canonicalJson(body),
    registry: {
      name: `tembra.hook/${descriptor.name}@${descriptor.version}`,
      conflict: "error" as const,
    },
    input: descriptor.input ?? undefined,
    result: descriptor.result ?? undefined,
    terminal:
      descriptor.terminal === null
        ? undefined
        : {
            input: descriptor.terminal.input ?? undefined,
            result: descriptor.terminal.result ?? undefined,
          },
    method: {
      kind: "method" as const,
      args: native(`tembra.hook.args/${digest}@1`),
      ...(result === undefined ? {} : { result }),
    },
  });
}

const { apply } = Reflect;
const { freeze } = Object;
const { defineProperty } = Object;
const { setPrototypeOf } = Object;
function assertHookArguments(value: unknown): asserts value is Record<string, unknown> {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    fail("hook_signature", "expected a hook argument record");
  }
}
function hookArguments(value: unknown): Record<string, unknown> {
  assertHookArguments(value);
  return value;
}
function invoke(target: unknown, args: unknown[]): unknown {
  if (typeof target !== "function") {
    fail("hook_signature", "expected an issued callable");
  }
  const result: unknown = apply(target, undefined, args);
  return result;
}
/**
 * Adapts a typed handler to the executor's checked raw view without executing it yet. A middleware
 * Next is scoped to its admitted invocation, can be consumed once, and requires explicit recover
 * after a primary failure. Backend wrappers bind authority and join physical Next work; keeping the
 * JavaScript function does not extend that scope. Handler completion may be synchronous or awaited.
 */
export function hookHandler<H extends AnyHookDefinition>(
  definition: H,
  // oxlint-disable-next-line promise/prefer-await-to-callbacks -- hook callbacks are executable contributions, not error-first asynchronous callbacks
  callback: NoInfer<HookHandler<H>>,
): HookHandler<H> {
  if (typeof callback !== "function") {
    fail("hook_signature", "hook handler must be callable");
  }
  const wrapped = (context: ManagedContext, raw: unknown) => {
    const value = hookArguments(raw);
    if (definition.descriptor.strategy !== "middleware") {
      return invoke(callback, [context, freeze({ input: value.input })]);
    }
    const nextRecord = hookArguments(value.next);
    const { call } = nextRecord;
    const { recover } = nextRecord;
    if (typeof call !== "function" || typeof recover !== "function") {
      fail("hook_signature", "expected issued call/recover functions");
    }
    const next = (input: unknown) => invoke(call, [input]);
    setPrototypeOf(next, null);
    defineProperty(next, "recover", {
      value: (fallback: unknown) => invoke(recover, [fallback]),
      enumerable: true,
    });
    return invoke(callback, [context, freeze({ input: value.input, next: freeze(next) })]);
  };
  // SAFETY: the executor validates the hook descriptor and raw arguments; this wrapper preserves that callback's generic input/result projection and issued Next functions.
  // oxlint-disable-next-line typescript/no-unsafe-type-assertion -- the runtime descriptor, rather than TypeScript, selects the generic hook argument shape
  return wrapped as HookHandler<H>;
}

/**
 * Declares member as a contribution under key during the synchronous object factory pass.
 * priority is a safe signed integer, default 0. Invalid options throw ContractError; actual
 * owner, lifetime and method-signature checks remain in core when the declaration is admitted.
 */
export function registerHook(
  self: ObjectDeclaration,
  definition: AnyHookDefinition,
  key: string,
  member: string,
  options: { readonly priority?: number } = {},
): void {
  if (Object.keys(options).some((optionKey) => optionKey !== "priority")) {
    fail("hook_priority", "unknown contribution option");
  }
  const priority = options.priority ?? 0;
  if (!Number.isSafeInteger(priority)) {
    fail("hook_priority", "priority must be a safe signed integer");
  }
  self.registerMethod(
    definition.registry,
    key,
    { hook: definition.descriptor.fingerprint, priority, profile: 1 },
    member,
  );
}

/** A hook run returns composition output, which differs from one handler's output. */
export type HookResult<H extends AnyHookDefinition> =
  HookStrategy extends H["descriptor"]["strategy"]
    ? unknown
    : H["descriptor"]["strategy"] extends "bail"
      ?
          | { readonly kind: "unresolved" }
          | { readonly kind: "resolved"; readonly value: Infer<H["result"]> }
      : H["terminal"] extends HookTerminalProfile
        ? Infer<H["terminal"]["result"]>
        : H["descriptor"]["strategy"] extends "parallel"
          ? readonly Infer<H["result"]>[]
          : H["descriptor"]["strategy"] extends "waterfall"
            ? Infer<H["input"]>
            : undefined;
/** Middleware and declared terminal profiles require an issued managed operation. */
export type HookRunOptions<H extends AnyHookDefinition> = undefined extends H["terminal"]
  ? HookTerminalProfile extends H["terminal"]
    ? { readonly terminal?: RegisteredOperation }
    : { readonly terminal?: never }
  : { readonly terminal: RegisteredOperation };
