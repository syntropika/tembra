/**
 * Private guest shim of a prepared source Orb.
 *
 * It adapts an evaluated `orb({...})` definition to the private `tembra:orb@0.1.0` controls and
 * to the Orb's own typed WIT exports. User values never cross the private package: service
 * methods, contributions, dependency calls and hook runs use the generic WIT bindings of the
 * public world. Only kernel state crosses as JSON text, because the kernel already stores it so.
 *
 * Authority stays in the host. The shim keeps callback epochs only to refuse stale guest objects
 * before a host call; every host operation is still checked against the native call group.
 */
import type * as Host from "./.tembra/tembra-orb-host.js";
import type * as Lifecycle from "./.tembra/tembra-orb-lifecycle.js";

type Json = null | boolean | number | string | readonly Json[] | { readonly [key: string]: Json };
type Callback = (...args: never[]) => unknown;

/** The fields of an `orb({...})` definition the shim reads. */
interface Definition {
  readonly name?: unknown;
  readonly apiVersion?: unknown;
  readonly create?: (self: unknown) => unknown;
  readonly services?: Readonly<
    Record<
      string,
      {
        readonly create?: (self: unknown) => unknown;
        readonly methods?: Readonly<Record<string, Callback>>;
        readonly lifetime?: unknown;
        readonly linger?: unknown;
        readonly access?: unknown;
      }
    >
  >;
  readonly hooks?: Readonly<Record<string, { readonly composition?: unknown }>>;
  readonly contributes?: Readonly<
    Record<string, Callback | { readonly handle?: Callback; readonly priority?: unknown }>
  >;
}

/** WIT interfaces the generated entry imports, keyed as the build resolved them. */
export interface Bindings {
  /** Callable Orb dependency imports, keyed by their WIT binding. */
  readonly dependencies: Readonly<Record<string, object>>;
  /** Self-imports of provided hooks, keyed by the hook alias. */
  readonly hooks: Readonly<Record<string, { readonly module: object; readonly method: string }>>;
}

/**
 * Per-method flags derived from the public WIT by the build: `a` async function, `r` result
 * function (informational; the runtime maps errors), `s` byte-stream result.
 */
export type MethodFlags = Readonly<Record<string, string>>;

export interface Shim {
  readonly lifecycle: typeof Lifecycle;
  /** Build the export object componentize-qjs reads for one exported public interface. */
  exported(binding: string, alias: string, methods: MethodFlags): Record<string, Callback>;
}

const CONTEXT = Symbol("tembra.context");

/** Module evaluation runs in the build-time snapshot, which has no host. */
const NO_HOST = "console output requires a callback; module evaluation has no host";

// The generated entry imports this module first, so author modules evaluate after it. Where the
// engine provides no console (the component runtime), refuse output with a clear error until
// `start` installs the callback console.
if (typeof (globalThis as { console?: unknown }).console === "undefined")
  Object.defineProperty(globalThis, "console", {
    configurable: true,
    value: Object.freeze(
      Object.fromEntries(
        ["log", "debug", "info", "warn", "error"].map((level) => [
          level,
          () => {
            throw new Error(NO_HOST);
          },
        ]),
      ),
    ),
  });
const RESOURCE = Symbol("tembra.resource");

/** Turn a host `err` (a string) into an Error without leaking other values. */
function failure(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

/** Call a host operation and rethrow its `err` as an Error. */
function checked<T>(operation: () => T): T {
  try {
    return operation();
  } catch (error) {
    throw failure(error);
  }
}

/** Await a host operation and reject with its `err` as an Error. */
async function settled<T>(operation: () => Promise<T>): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    throw failure(error);
  }
}

/**
 * Run the typed import that consumes a pending selection. A synchronous throw means the runtime
 * refused the arguments before the host import was entered, so the selection is still pending:
 * clear it and rethrow. Never clear on success or later: an asynchronous import's host entry
 * consumes its own selection, possibly after this turn.
 */
function selected<T>(call: () => T, clear: () => void): T {
  try {
    return call();
  } catch (error) {
    try {
      clear();
    } catch {
      // The original refusal is the author's error. A failed clear leaves the selection
      // pending, and the host then refuses the next operation by itself.
    }
    throw error;
  }
}

/** The `err` string of a `result<_, string>` lifecycle export. */
function reason(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function thenable(value: unknown): boolean {
  return (
    (typeof value === "object" || typeof value === "function") &&
    value !== null &&
    typeof (value as { then?: unknown }).then === "function"
  );
}

/** Copy and freeze JSON data without silently changing dates, functions, integers or cycles. */
function protect(value: unknown, ancestors = new Set<object>(), depth = 0): Json {
  if (depth > 64) throw new Error("State nesting exceeds the supported profile");
  if (value === null || typeof value === "string" || typeof value === "boolean") return value;
  if (
    typeof value === "number" &&
    Number.isFinite(value) &&
    !Object.is(value, -0) &&
    (!Number.isInteger(value) || Number.isSafeInteger(value))
  )
    return value;
  if (typeof value !== "object" || ancestors.has(value))
    throw new Error("State requires finite JSON data without cycles");
  if (
    !Array.isArray(value) &&
    Object.getPrototypeOf(value) !== Object.prototype &&
    Object.getPrototypeOf(value) !== null
  )
    throw new Error("State objects require a JSON data codec");
  ancestors.add(value);
  const copy = Array.isArray(value)
    ? Array.from(value, (item) => protect(item, ancestors, depth + 1))
    : Object.fromEntries(
        Object.entries(value).map(([name, item]) => [name, protect(item, ancestors, depth + 1)]),
      );
  ancestors.delete(value);
  return Object.freeze(copy) as Json;
}

interface Readable {
  readonly value: Json;
  select(compute: (value: Json) => unknown): Readable;
}

/** Derive values only when the retained snapshot changes. */
function selection(source: { readonly value: Json }, compute: (value: Json) => unknown): Readable {
  let previous: unknown = Symbol();
  let previousResult: unknown = Symbol();
  let selected: Json = null;
  const view: Readable = Object.freeze({
    get value() {
      const value = source.value;
      if (!Object.is(previous, value)) {
        const next = compute(value);
        if (!Object.is(previousResult, next)) selected = protect(next);
        previous = value;
        previousResult = next;
      }
      return selected;
    },
    select(next: (value: Json) => unknown) {
      return selection(view, next);
    },
  });
  return view;
}

/**
 * Retain snapshot identity until its kernel revision changes. Each read names the retained
 * revision so the host may omit an unchanged value; a changed revision must carry its value.
 */
function readable(read: (known: string | undefined) => Host.Snapshot) {
  let revision: string | undefined;
  let value: Json = null;
  const binding = {
    adopt(snapshot: Host.Snapshot): Json {
      if (revision !== snapshot.revision) {
        if (snapshot.value === undefined) throw new Error("Host omitted a changed state value");
        value = protect(JSON.parse(snapshot.value));
        revision = snapshot.revision;
      }
      return value;
    },
    get value(): Json {
      return binding.adopt(read(revision));
    },
    select(compute: (value: Json) => unknown): Readable {
      return selection(binding, compute);
    },
  };
  return Object.freeze(binding);
}

/** Normalize a key: the host carries lists, a string names a one-segment key. */
function keyList(key: unknown): string[] {
  if (typeof key === "string") return [key];
  if (!Array.isArray(key) || key.some((part) => typeof part !== "string"))
    throw new TypeError("a reference key is a string or a list of strings");
  return [...key];
}

/**
 * Install the shim for one evaluated definition. Called once by the generated entry; `global`
 * receives the author globals `console` and `tembra`.
 */
export function start(
  host: typeof Host,
  definition: Definition,
  bindings: Bindings,
  global: object = globalThis,
): Shim {
  if (!definition || typeof definition !== "object")
    throw new Error("The Orb module must export a default orb definition");
  const environments = new Map<bigint, unknown>();
  const callbacks = new Map<number, { owner: bigint; run: (...args: never[]) => unknown }>();
  const values = new Map<number, { owner: bigint; value: unknown }>();
  let next = 1;
  /** The environment whose factory is running; declarations are refused otherwise. */
  let declaring: bigint | undefined;
  /** The epoch of the export call that is running, or undefined between calls. */
  let active: object | undefined;
  /** Module evaluation has no host authority; it ends with the first admitted callback. */
  let evaluated = false;

  /** Run one export call under a fresh epoch; asynchronous calls keep it until they settle. */
  function enter<T>(asynchronous: boolean, run: (epoch: object) => T): T {
    evaluated = true;
    const epoch = {};
    active = epoch;
    const leave = () => {
      if (active === epoch) active = undefined;
    };
    if (!asynchronous) {
      try {
        return run(epoch);
      } finally {
        leave();
      }
    }
    return (async () => {
      try {
        return await run(epoch);
      } finally {
        leave();
      }
    })() as T;
  }

  function retain(owner: bigint, run: (...args: never[]) => unknown): number {
    const id = next++;
    callbacks.set(id, { owner, run });
    return id;
  }

  function handle(scope: bigint, id: number): Host.Handle {
    return { scope, id };
  }

  /** Refuse a guest object captured in another or an ended callback, before any host call. */
  function live(epoch: object | undefined): void {
    if (epoch === undefined || active !== epoch) throw new Error("Context callback has ended");
  }

  /** Expose the current context's operations, bound to the epoch of the callback creating it. */
  function context(environment: bigint) {
    const epoch = active;
    return Object.freeze({
      [CONTEXT]: () => live(epoch),
      get principal() {
        live(epoch);
        return checked(() => host.contextPrincipal());
      },
      check() {
        live(epoch);
        checked(() => host.contextCheck());
      },
      cancel() {
        live(epoch);
        checked(() => host.contextCancel());
      },
      /** Acquire a selected WIT Orb import by key for this context's root and principal. */
      async get(binding: string, key: unknown, options?: unknown) {
        live(epoch);
        if (options !== undefined)
          throw new Error("Acquisition options are unsupported by the isolated value profile");
        const id = await settled(() => host.referenceAcquire(binding, keyList(key)));
        return reference(environment, id, binding);
      },
    });
  }

  /** Check a context argument issued by this shim and still inside its callback. */
  function issued(value: unknown): void {
    const check = (value as { [CONTEXT]?: () => void } | null | undefined)?.[CONTEXT];
    if (typeof check !== "function") throw new Error("Hook calls require an issued context");
    check();
  }

  /** Proxy one host reference to a WIT dependency import. */
  function reference(scope: bigint, id: number, binding: string) {
    const module = bindings.dependencies[binding] as Record<string, unknown> | undefined;
    const target = handle(scope, id);
    const state = readable((known) => checked(() => host.referenceState(target, known)));
    return new Proxy(Object.create(null) as object, {
      get(_target, member) {
        if (member === "then") return undefined;
        if (member === "release") return () => checked(() => host.referenceRelease(target));
        if (member === "ready") return settled(() => host.referenceReady(target));
        if (member === "status") return checked(() => host.referenceStatus(target));
        if (member === "state") return state;
        if (typeof member !== "string") return undefined;
        const method = module?.[member];
        if (typeof method !== "function")
          throw new Error(`Unknown WIT import method ${binding}#${member}`);
        return (...args: unknown[]) => {
          const last = args.at(-1) as { [CONTEXT]?: () => void } | undefined;
          if (typeof last?.[CONTEXT] === "function") {
            last[CONTEXT]();
            args.pop();
          }
          // The selection and its typed import form one synchronous turn; nothing may intervene.
          checked(() => host.select(target));
          return selected(
            () => (method as Callback).apply(module, args as never[]),
            () => host.select(undefined),
          );
        };
      },
    });
  }

  /** Create the declaration of one environment's factory. */
  function declaration(owner: bigint) {
    function factory() {
      if (declaring !== owner) throw new Error("Declaration factory has ended");
    }
    /**
     * Metadata belongs to the declaration's own environment. The host answers for the running
     * callback's environment, so a declaration retained into another environment is refused.
     */
    function owned() {
      if (checked(() => host.environment()) !== owner)
        throw new Error("Declaration metadata is readable only in its own environment");
    }
    function address() {
      owned();
      return checked(() => host.instanceAddress());
    }
    return Object.freeze({
      get key() {
        return Object.freeze([...address().key]);
      },
      get principal() {
        return address().principal;
      },
      get orb() {
        owned();
        const root = checked(() => host.root());
        return root === undefined ? undefined : environments.get(root);
      },
      state(initial: unknown, options: { version?: number; validate?(value: Json): unknown } = {}) {
        factory();
        const validate = (value: unknown): Json => {
          if (thenable(value)) throw new Error("State must be synchronous");
          const data = protect(value);
          const result = options.validate?.(data);
          if (result !== undefined && result !== true) throw new Error("State validation failed");
          return data;
        };
        const first = validate(initial);
        const check = options.validate
          ? retain(owner, (value: never) => {
              validate(value);
            })
          : undefined;
        const id = checked(() =>
          host.stateDeclare(JSON.stringify(first), options.version ?? 1, check),
        );
        const target = handle(owner, id);
        const view = readable((known) => checked(() => host.stateRead(target, known)));
        if (check !== undefined) validate(view.value);
        return Object.freeze({
          get value() {
            return view.value;
          },
          select(compute: (value: Json) => unknown) {
            return view.select(compute);
          },
          update(context: { check(): void }, change: (value: Json) => unknown) {
            context.check();
            const value = validate(change(view.value));
            return view.adopt(checked(() => host.stateUpdate(target, JSON.stringify(value))));
          },
        });
      },
      hook(name: string) {
        factory();
        const bound = bindings.hooks[name];
        if (!bound || definition.hooks?.[name] === undefined)
          throw new Error(`Unknown WIT hook ${name}`);
        const method = (bound.module as Record<string, unknown>)[bound.method];
        if (typeof method !== "function") throw new Error(`Unknown WIT hook ${name}`);
        const target = handle(
          owner,
          checked(() => host.hookDeclare(name)),
        );
        return Object.freeze({
          run(ctx: unknown, input: unknown) {
            issued(ctx);
            (ctx as { check(): void }).check();
            checked(() => host.selectHook(target));
            return selected(
              () => (method as Callback).call(bound.module, input as never),
              () => host.selectHook(undefined),
            );
          },
        });
      },
      get(binding: string, key: unknown = ["main"], options?: unknown) {
        factory();
        if (options !== undefined)
          throw new Error("Acquisition options are unsupported by the isolated value profile");
        if (!(binding in bindings.dependencies))
          throw new Error(`The Orb contract does not import ${binding}`);
        const id = checked(() => host.referenceDeclare(binding, keyList(key)));
        return reference(owner, id, binding);
      },
      resource(
        name: string,
        options?: {
          version?: number;
          exclusive?: boolean;
          opaque?: boolean;
          deps?: readonly unknown[];
          open(signal: { readonly aborted: boolean; throwIfAborted(): void }): unknown;
          close?(value: unknown, context: unknown): unknown;
        },
      ) {
        factory();
        if (options === undefined || typeof options.open !== "function")
          throw new Error("Named host resources require an opener");
        if (options.exclusive || options.opaque)
          throw new Error("This adapter does not yet support these resource options");
        const dependencies = (options.deps ?? []).map((resource) => {
          const key = (resource as { [RESOURCE]?: Host.Handle } | null)?.[RESOURCE];
          if (!key) throw new Error("Resource dependencies must be issued resource handles");
          return key;
        });
        const open = retain(owner, async () => {
          const epoch = active;
          const cancelled = () => active !== epoch || checked(() => host.signalCancelled());
          const value = await options.open(
            Object.freeze({
              get aborted() {
                return cancelled();
              },
              throwIfAborted() {
                if (cancelled()) throw new Error("Operation cancelled");
              },
            }),
          );
          const slot = next++;
          values.set(slot, { owner, value });
          return slot;
        });
        const close = retain(owner, (async (slot: number) => {
          const entry = values.get(slot);
          try {
            await options.close?.(entry?.value, context(owner));
          } finally {
            values.delete(slot);
          }
        }) as never);
        const version = options.version ?? 1;
        const target = handle(
          owner,
          checked(() => host.resourceDeclare(name, version, open, close, dependencies)),
        );
        return Object.freeze({
          [RESOURCE]: target,
          version,
          get value() {
            return values.get(checked(() => host.resourceRead(target)))?.value;
          },
        });
      },
    });
  }

  /** The evaluated definition, as the host checks it before any factory runs. */
  function describe(): Lifecycle.Implementation {
    const { name, apiVersion } = definition;
    if (typeof name !== "string" || !Number.isInteger(apiVersion) || (apiVersion as number) <= 0)
      throw new Error("An Orb requires a literal name and positive apiVersion");
    const services = Object.entries(definition.services ?? {}).map(([alias, service]) => {
      if (
        service.lifetime !== undefined &&
        service.lifetime !== "referenced" &&
        service.lifetime !== "host"
      )
        throw new Error(`${alias}: unsupported lifetime`);
      if (
        service.linger !== undefined &&
        (!Number.isInteger(service.linger) || (service.linger as number) < 0)
      )
        throw new Error(`${alias}: linger must be a non-negative integer`);
      return {
        alias,
        methods: Object.keys(service.methods ?? {}),
        ...(service.lifetime === undefined
          ? {}
          : { lifetime: service.lifetime as Lifecycle.Lifetime }),
        ...(service.linger === undefined ? {} : { linger: service.linger as number }),
        access: !!service.access,
      };
    });
    const hooks = Object.entries(definition.hooks ?? {}).map(([alias, hook]) => {
      const composition = hook?.composition;
      if (composition !== "series" && composition !== "waterfall" && composition !== "bail")
        throw new Error("Portable hooks require a supported composition");
      return { alias, composition: composition as Lifecycle.Composition };
    });
    const contributes = Object.entries(definition.contributes ?? {}).map(
      ([binding, contribution]) => {
        const run = typeof contribution === "function" ? contribution : contribution?.handle;
        if (typeof run !== "function")
          throw new Error("A contribution requires an executable callback");
        const priority = typeof contribution === "function" ? 0 : (contribution.priority ?? 0);
        if (!Number.isInteger(priority)) throw new Error("A contribution priority is an integer");
        return { binding, priority: priority as number };
      },
    );
    return { name, apiVersion: apiVersion as number, services, hooks, contributes };
  }

  const lifecycle: typeof Lifecycle = Object.freeze({
    describe,
    create(environment: bigint, service: string | undefined) {
      enter(false, () => {
        try {
          const factory =
            service === undefined
              ? definition.create
              : definition.services?.[service]
                ? definition.services[service]!.create
                : (() => {
                    throw new Error(`Unknown service ${service}`);
                  })();
          declaring = environment;
          let value: unknown;
          try {
            value = factory?.(declaration(environment)) ?? {};
          } finally {
            declaring = undefined;
          }
          if (thenable(value)) throw new Error("Factories must be synchronous");
          environments.set(environment, value);
        } catch (error) {
          throw reason(error);
        }
      });
    },
    release(environment: bigint) {
      enter(false, () => {
        environments.delete(environment);
        for (const [id, callback] of callbacks)
          if (callback.owner === environment) callbacks.delete(id);
        for (const [slot, entry] of values) if (entry.owner === environment) values.delete(slot);
      });
    },
    validate(callback: number, value: string) {
      enter(false, () => {
        try {
          const run = callbacks.get(callback)?.run;
          if (!run) throw new Error("Unknown state validator");
          const result = (run as (value: unknown) => unknown)(JSON.parse(value));
          if (thenable(result)) throw new Error("State validators must be synchronous");
        } catch (error) {
          throw reason(error);
        }
      });
    },
    open(callback: number) {
      return enter(true, async () => {
        try {
          const run = callbacks.get(callback)?.run;
          if (!run) throw new Error("Unknown resource opener");
          return (await (run as () => unknown)()) as number;
        } catch (error) {
          throw reason(error);
        }
      });
    },
    close(callback: number, slot: number) {
      return enter(true, async () => {
        try {
          const run = callbacks.get(callback)?.run;
          if (!run) throw new Error("Unknown resource closer");
          await (run as (slot: number) => unknown)(slot);
        } catch (error) {
          throw reason(error);
        }
      });
    },
  });

  /** Keep the export epoch until a returned byte stream has been produced completely. */
  function produce(epoch: object, stream: unknown): AsyncIterable<unknown> {
    if (
      (typeof stream !== "object" && typeof stream !== "function") ||
      stream === null ||
      (!(Symbol.asyncIterator in stream) && !(Symbol.iterator in stream))
    )
      throw new TypeError("A stream result requires an iterable of Uint8Array chunks");
    return (async function* () {
      active = epoch;
      try {
        yield* stream as AsyncIterable<unknown>;
      } finally {
        if (active === epoch) active = undefined;
      }
    })();
  }

  function exported(binding: string, alias: string, methods: MethodFlags) {
    const wrappers: Record<string, Callback> = {};
    for (const [method, flags] of Object.entries(methods)) {
      const asynchronous = flags.includes("a");
      const stream = flags.includes("s");
      /** Resolve the author callback and the environment it receives. */
      function target(): [Callback, unknown, bigint] {
        const environment = checked(() => host.environment());
        const contribution = definition.contributes?.[binding];
        if (contribution !== undefined) {
          const run = typeof contribution === "function" ? contribution : contribution.handle;
          if (typeof run !== "function") throw new Error("Unknown contribution callback");
          return [run, environments.get(environment), environment];
        }
        const run = definition.services?.[alias]?.methods?.[method];
        if (typeof run !== "function") throw new Error(`Unknown service method ${alias}.${method}`);
        return [run, environments.get(environment), environment];
      }
      wrappers[method] = asynchronous
        ? (...args: unknown[]) =>
            // Author errors reach the runtime unchanged: it maps them to `err` (an own `payload`,
            // or the message for a `string` error) or traps.
            enter(true, async (epoch) => {
              const [run, environment, id] = target();
              const value = await (run as (...args: unknown[]) => unknown)(
                environment,
                context(id),
                ...args,
              );
              return stream ? produce(epoch, value) : value;
            })
        : (...args: unknown[]) =>
            enter(false, () => {
              const [run, environment, id] = target();
              const value = (run as (...args: unknown[]) => unknown)(
                environment,
                context(id),
                ...args,
              );
              if (thenable(value))
                throw new TypeError(
                  `${alias}.${method} is a synchronous WIT function; declare it async in WIT to return a Promise`,
                );
              return value;
            });
    }
    return Object.freeze(wrappers);
  }

  const levels = ["log", "debug", "info", "warn", "error"] as const;
  Object.defineProperty(global, "console", {
    configurable: true,
    writable: false,
    value: Object.freeze(
      Object.fromEntries(
        levels.map((level) => [
          level,
          (...args: unknown[]) => {
            if (!evaluated) throw new Error(NO_HOST);
            const message = args
              .map((value) => (typeof value === "string" ? value : String(value)))
              .join(" ");
            checked(() => host.log(level, message));
          },
        ]),
      ),
    ),
  });
  Object.defineProperty(global, "tembra", {
    configurable: false,
    writable: false,
    value: Object.freeze({
      wait(milliseconds: number) {
        const bounded = Math.min(Math.max(0, Math.ceil(Number(milliseconds) || 0)), 0xffff_ffff);
        return settled(() => host.wait(bounded));
      },
    }),
  });

  return Object.freeze({ lifecycle, exported });
}
