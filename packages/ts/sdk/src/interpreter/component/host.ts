/**
 * Typed Orb dependency imports of a compiled component.
 *
 * Each call acts for the callback that owns the guest's current root export task (read
 * synchronously at import entry) and stays physically accounted to that callback until it
 * settles. A plain import targets the root environment's default `main` reference; a reference
 * view's methods target the claim their receiver names. Infrastructure failures (refusal,
 * cancellation, a provider failure, a stale or foreign receiver) reject, which the generated
 * suspending import turns into a trap: only the function's own WIT `result` is recoverable. A
 * view's acquisition is the exception: its key is request data, so any failure to acquire is
 * its WIT `err`, never a trap.
 */
import type { ResourceCapability, ByteStreamCapability } from "#tembra-adapters";
import { receiveByteStream } from "./streams.ts";
import type { ComponentPort } from "./port.ts";
import type { Described } from "./revision.ts";
import type { ReferenceBinding } from "../types.ts";
import type { ReferenceClaim } from "./scope.ts";
import type { FunctionValues, GraphFunction } from "./values.ts";

/** One typed import function and its conversions. */
export interface ImportedFunction {
  /** The WIT function name. */
  readonly name: string;
  /** The JavaScript spelling the generated bindings call. */
  readonly spelling: string;
  readonly values: FunctionValues;
}

/** The import object of one Orb dependency binding. */
export function dependencyImport(
  port: ComponentPort,
  binding: string,
  functions: readonly ImportedFunction[],
): Record<string, unknown> {
  return Object.fromEntries(
    functions.map(({ spelling, values }) => [
      spelling,
      (...args: unknown[]) => {
        const callback = port.current();
        const input = values.hostArguments(args);
        return port
          .physical(callback, callback.scope.call(binding, spelling, input))
          .then(values.toGuest);
      },
    ]),
  );
}

/**
 * A view method's data signature: the implicit `self` receiver names a host-local reference and
 * never travels as data, while the method keeps its original WIT slot.
 */
export function viewMethod<F extends { readonly params: readonly unknown[] }>(fn: F): F {
  if (!fn.params.length) throw new Error("A reference view method has no receiver");
  return { ...fn, params: fn.params.slice(1) };
}

/**
 * The generated bindings' spelling of one WIT identifier: kebab-case words, each entirely lower
 * or upper case, joined in lower (functions, methods) or upper (resource classes) camel case.
 */
export function bindingName(name: string, upper: boolean): string {
  return name
    .split("-")
    .map((word, index) => {
      const lower = word.toLowerCase();
      return index === 0 && !upper ? lower : lower.charAt(0).toUpperCase() + lower.slice(1);
    })
    .join("");
}

/** The checked shape of one reference-view import, with its generated JavaScript spellings. */
export interface ReferenceView {
  /** The generated binding's name of the view's host resource class. */
  readonly resource: string;
  /** The acquisition function; its conversions cover only the key parameter. */
  readonly acquire: ImportedFunction;
  /** Methods by original WIT name, with conversions of their data parameters (no `self`). */
  readonly methods: readonly ImportedFunction[];
}

/**
 * The generated spellings and data conversions of one checked reference view: the resource
 * class, the acquisition function (its key only) and each method without its `self` receiver.
 */
export function referenceView(
  graph: Described,
  binding: string,
  view: ReferenceBinding,
  convert: (fn: GraphFunction) => FunctionValues,
): ReferenceView {
  const functions = graph.imports.find((item) => item.binding === binding)?.functions ?? {};
  const acquire = functions[view.acquire];
  if (!acquire) throw new Error(`Missing prepared signature for ${binding}#${view.acquire}`);
  const spelling = graph.javascriptNames[view.acquire];
  if (spelling === undefined) throw new Error(`Missing WIT JavaScript name for ${view.acquire}`);
  const prefix = `[method]${view.resource.name}.`;
  const names = Object.keys(functions);
  if (names.length !== Object.keys(view.methods).length + 1)
    throw new Error(`The reference view ${binding} declares unchecked functions`);
  return {
    resource: bindingName(view.resource.name, true),
    // The acquisition result is a host-local handle, converted by the view itself.
    acquire: {
      name: view.acquire,
      spelling,
      values: convert({ params: acquire.params } as GraphFunction),
    },
    methods: Object.keys(view.methods).map((name) => {
      const fn = functions[name];
      if (!fn || !name.startsWith(prefix))
        throw new Error(`The reference view ${binding} lacks method ${name}`);
      return {
        name,
        spelling: bindingName(name.slice(prefix.length), false),
        values: convert(viewMethod(fn) as GraphFunction),
      };
    }),
  };
}

/** The text of an acquisition failure the guest receives as its WIT `err`. */
function failure(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * The import object of one reference-view binding: its acquisition function and the host
 * resource class the generated bindings require.
 *
 * An instance is a host-local capability living only in this instance's generated handle table:
 * it names one root claim and is never an inter-Orb guest resource. The guest dropping its own
 * handle releases the claim; the claim's root ending invalidates it.
 */
export function referenceImport(
  port: ComponentPort,
  binding: string,
  view: ReferenceView,
): Record<string, unknown> {
  /** Live views; an object outside this map is dropped or was never issued. */
  const held = new WeakMap<object, ReferenceClaim>();
  const claimOf = (value: unknown): ReferenceClaim => {
    const claim = typeof value === "object" && value !== null ? held.get(value) : undefined;
    if (!claim) throw new Error("The reference handle is not live");
    return claim;
  };
  const Resource = class {
    constructor(claim: ReferenceClaim) {
      held.set(this, claim);
    }

    /**
     * The guest dropped its own handle, synchronously inside its drop. The release runs after the
     * drop returns, accounted to the dropping callback, which settles only once it is done.
     */
    [Symbol.dispose](): void {
      const claim = held.get(this);
      if (!claim) return;
      held.delete(this);
      let callback;
      try {
        callback = port.current();
      } catch {
        // Outside any admitted callback the claim stays with its root, which releases it.
        return;
      }
      port.wasi.cleanup(() => callback.scope.release(claim));
    }
  };
  for (const { name, spelling, values } of view.methods) {
    if (Object.hasOwn(Resource.prototype, spelling))
      throw new Error(`Ambiguous reference method ${spelling}`);
    Object.defineProperty(Resource.prototype, spelling, {
      value(this: unknown, ...args: unknown[]) {
        const claim = claimOf(this);
        const callback = port.current();
        const input = values.hostArguments(args);
        return port
          .physical(callback, callback.scope.invoke(claim, binding, name, input))
          .then(values.toGuest);
      },
    });
  }
  const { values: key } = view.acquire;
  const acquire = (...args: unknown[]) => {
    const callback = port.current();
    const [input] = key.hostArguments(args) as [readonly string[]];
    const abandoned = new AbortController();
    let issued: object | undefined;
    const pending = port.physical(
      callback,
      callback.scope.acquire(binding, input, abandoned.signal).then(
        (claim) => {
          if (abandoned.signal.aborted) {
            callback.scope.release(claim);
            return { tag: "err", val: "acquisition abandoned" };
          }
          issued = new Resource(claim);
          return { tag: "ok", val: issued };
        },
        (error: unknown) => ({ tag: "err", val: failure(error) }),
      ),
    );
    // The generated bindings dispose a host result the guest abandoned before delivery: stop a
    // pending readiness wait, or release a claim that was ready but never delivered.
    return Object.assign(pending, {
      [Symbol.dispose]() {
        abandoned.abort(new Error("acquisition abandoned"));
        const claim = issued && held.get(issued);
        if (!claim) return;
        held.delete(issued!);
        callback.scope.release(claim);
      },
    });
  };
  return { [view.resource]: Resource, [view.acquire.spelling]: acquire };
}

/** Ordinary provider-owned WIT resources; keyed reference views use their separate adapter. */
export function resourceImport(
  port: ComponentPort,
  binding: string,
  functions: readonly (ImportedFunction & {
    readonly roles: import("./resources.ts").ResourceRoles;
  })[],
): Record<string, unknown> {
  const held = new WeakMap<object, ResourceCapability>();
  const classes = new Map<string, { new (...args: unknown[]): object }>();
  const ownerOf = (value: object) => {
    const owner = held.get(value);
    if (!owner) throw new Error("Resource is not live");
    return owner;
  };
  const invoke = (
    fn: (typeof functions)[number],
    args: unknown[],
    receiver?: ResourceCapability,
  ): Promise<unknown> & Disposable => {
    const callback = port.current();
    const input = fn.values.hostArguments(args);
    let abandoned = false;
    let issued: ResourceCapability | undefined;
    const pending = port.physical(
      callback,
      (async () => {
        const result = receiver
          ? await callback.scope.resource!(receiver, binding, fn.spelling, input)
          : await callback.scope.call(binding, fn.spelling, input);
        if (fn.roles.own) {
          const owner = result as ResourceCapability;
          if (owner?.kind !== "resource") throw new Error("Missing owned resource result");
          issued = owner;
          if (abandoned) {
            owner.close();
            throw new Error("Resource delivery abandoned");
          }
          const Resource = classes.get(fn.roles.own.name);
          if (!Resource) {
            owner.close();
            throw new Error("Missing imported resource class");
          }
          const value = Object.create(Resource.prototype) as object;
          held.set(value, owner);
          return value;
        }
        if (fn.roles.stream) {
          // The stream's host adapter strips producer-private JCO representations.
          return receiveByteStream(
            result as ByteStreamCapability,
            AbortSignal.any([
              callback.ended.signal,
              ...(callback.scope.signal ? [callback.scope.signal] : []),
            ]),
            (operation) => port.wasi.cleanup(operation),
            (error) => callback.scope.report(error),
          );
        }
        return fn.values.toGuest(result);
      })(),
    );
    return Object.assign(pending, {
      [Symbol.dispose]() {
        abandoned = true;
        issued?.close();
      },
    });
  };
  for (const fn of functions) {
    const nominal = fn.roles.resource ?? fn.roles.own;
    if (!nominal || classes.has(nominal.name)) continue;
    const constructor = functions.find(
      (item) => item.roles.construct && item.roles.resource?.name === nominal.name,
    );
    const Resource = class {
      constructor(...args: unknown[]) {
        if (!constructor) throw new Error("Resource has no constructor");
        return invoke(constructor, args) as never;
      }
      [Symbol.dispose](): void {
        const owner = held.get(this);
        if (!owner) return;
        held.delete(this);
        try {
          port.current().scope.releaseResource(owner);
        } catch {
          owner.close();
        }
      }
    };
    classes.set(nominal.name, Resource);
  }
  const output: Record<string, unknown> = {};
  for (const [name, Resource] of classes) output[bindingName(name, true)] = Resource;
  for (const fn of functions) {
    if (fn.roles.construct) continue;
    if (!fn.roles.resource) {
      output[fn.spelling] = (...args: unknown[]) => invoke(fn, args);
      continue;
    }
    const Resource = classes.get(fn.roles.resource.name)!;
    const target = fn.roles.receiver ? Resource.prototype : Resource;
    const spelling = bindingName(fn.roles.member!, false);
    if (Object.hasOwn(target, spelling)) throw new Error("Ambiguous imported resource method");
    Object.defineProperty(target, spelling, {
      value(this: object, ...args: unknown[]) {
        return invoke(fn, args, fn.roles.receiver ? ownerOf(this) : undefined);
      },
    });
  }
  return output;
}
