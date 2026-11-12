/**
 * The kernel's own interfaces (`tembra:guest`) for a prepared component.
 *
 * A compiled Orb keeps kernel-managed state, opens root resources, declares hooks and reads its
 * call's context through the bundled `tembra:guest` package. Native hosts serve it by turning
 * each operation into the private host request a source Orb issues (`state.declare`,
 * `resource.declare`, `hook.declare`, `hook.run`, ...), answered by the same code as for a
 * source Orb: the native executor on Rust hosts, the interpreter `Scope` on Node and mobile. A
 * prepared browser component makes the same translation and lends its tables to that same
 * `Scope`, so one Orb keeps one behavior on every host:
 *
 * - While each environment is declared, the root binds the hooks this Orb runs and declares the
 *   resources its component lists, with callback numbers of its own; a service object declares
 *   the state its component describes. The address of each environment is read then.
 * - The kernel later opens and closes resources through those numbers and, before a reload
 *   adopts a retained state, asks the replacement component to validate it.
 * - Imports issue `state.read`, `state.update`, `resource.read`, the context requests and
 *   `hook.run` under the authority of the callback that physically issued them; a refusal
 *   rejects the suspending import, which traps the guest.
 *
 * Mirrors the component worker of `crates/executor-wasm/src/component/worker.rs`.
 */
import type { Declaration } from "../../authoring/declaration.ts";
import type { Gate } from "../gate.ts";
import { encode, VALUE_LIMIT, type Encoded } from "../codec.ts";
import type { LogLevel } from "../host-protocol.ts";
import { Scope, type Authority, type ScopeRevision } from "../scope.ts";
import type { ContractPort, ExecutionScope, HookBinding } from "../types.ts";
import { lift, type Signature } from "../wit-shape.ts";
import type { ComponentPort } from "./port.ts";
import type { Described } from "./revision.ts";
import type { CallbackScope, ComponentScope } from "./scope.ts";
import { converters, type FunctionValues, type GraphFunction } from "./values.ts";

/** Kernel interface identities; `crates/contracts/src/guest.rs` names the same. */
export const STATE = "tembra:guest/state@1.0.0";
export const STATE_INIT = "tembra:guest/state-init@1.0.0";
export const RESOURCES = "tembra:guest/resources@1.0.0";
export const RESOURCES_INIT = "tembra:guest/resources-init@1.0.0";
export const CONTEXT = "tembra:guest/context@1.0.0";

/** Whether an interface identity belongs to the namespace reserved for the kernel. */
export function isKernel(contract: string | null | undefined): boolean {
  return contract?.startsWith("tembra:") ?? false;
}

const LEVELS: readonly string[] = ["log", "debug", "info", "warn", "error"] satisfies LogLevel[];

/** The kernel handles of one environment, issued from its own `Scope` table. */
export interface KernelTable {
  /** Serves the environment's requests exactly as for a source Orb. */
  readonly scope: Scope;
  /** The Orb root's table, which owns the resources and hooks; the root's own for the root. */
  readonly root: KernelTable;
  /** The state handle of a service object that declared state. */
  state?: number;
  /** The root's resources by name. */
  readonly resources: Map<string, number>;
  /** The root's hook handles by the import binding that runs each hook. */
  readonly hooks: Map<string, number>;
  /** Where the environment's object lives, read while it was declared. */
  address?: unknown;
  /** Whether the component imports the context interface: an admitted call reads its principal. */
  readonly context: boolean;
}

/** One callback's access to its environment's table, under the callback's own authority. */
export interface KernelAccess {
  readonly table: KernelTable;
  readonly execution: ExecutionScope;
  /**
   * The principal of an admitted call, read when the call was admitted, for a component importing
   * the context interface: the kernel refuses to read it once the call is cancelled, and it never
   * changes.
   */
  readonly principal?: string | undefined;
}

/** A number this revision issued in a declaration, which the kernel calls back later. */
type Numbered =
  | { readonly kind: "open" | "close"; readonly environment: number; readonly name: string }
  | { readonly kind: "validate"; readonly environment: number; readonly service: string };

/** One function of a kernel interface the component exports, with its conversions. */
interface Exported {
  readonly binding: string;
  readonly spelling: string;
  readonly values: FunctionValues;
}

/** What the kernel reads of its component revision. */
export interface KernelOwner {
  readonly port: ComponentPort;
  readonly contract: ContractPort;
  readonly gate: Gate;
  readonly graph: Described;
  readonly signatures: Map<number, Signature>;
  readonly scopes: ReadonlyMap<number, ComponentScope>;
}

/** Where a compiled Orb's diagnostic records go, as for a source Orb's console. */
export type LogObserver = (record: { orb: string; level: string; message: string }) => void;

const isU32 = (value: unknown): value is number =>
  Number.isInteger(value) && (value as number) >= 0 && (value as number) <= 0xffff_ffff;

/**
 * Parse JSON text a component wrote, naming what it was in the refusal. A negative zero becomes
 * zero, as `JSON.stringify` writes it and as the native worker stores it: the portable value
 * profile has none, so a retained `-0` could never be validated or encoded again.
 */
function json(text: unknown, what: string): unknown {
  if (typeof text !== "string") throw new Error(`${what} is not JSON text`);
  try {
    return JSON.parse(text, (_key, value: unknown) => (Object.is(value, -0) ? 0 : value));
  } catch (error) {
    throw new Error(`${what} is not JSON: ${(error as Error).message}`);
  }
}

/** The kernel's interfaces for one prepared component revision. */
export class ComponentKernel {
  /** The source scopes serving each environment's requests; a service's parent is found here. */
  readonly #scopes = new Map<number, Scope>();
  /** The hooks this Orb declares, by the alias `hook.declare` names. */
  readonly #hooks = new Map<string, HookBinding>();
  /** The import binding that runs each declared hook, to that hook's alias. */
  readonly #runs = new Map<string, string>();
  readonly #numbers = new Map<number, Numbered>();
  #next = 0;
  /** Kernel callbacks the component exports, as `interface#function`. */
  readonly #exports = new Map<string, Exported>();
  readonly #convert: (fn: GraphFunction) => FunctionValues;
  /** The tables `Scope` serves this revision's requests with. */
  readonly #revision: ScopeRevision;
  /** Whether the component imports the context interface: every environment reads its address. */
  readonly #context: boolean;
  #used: boolean;

  constructor(
    readonly owner: KernelOwner,
    readonly orb: string,
    readonly log: LogObserver | undefined,
  ) {
    const graph = owner.graph;
    this.#convert = converters(graph);
    for (const entry of graph.exports) {
      if (entry.contract !== STATE_INIT && entry.contract !== RESOURCES_INIT) continue;
      for (const [name, fn] of Object.entries(entry.functions ?? {})) {
        const spelling = graph.javascriptNames[name];
        if (spelling === undefined) throw new Error(`Missing WIT JavaScript name for ${name}`);
        this.#exports.set(`${entry.contract}#${name}`, {
          binding: entry.binding,
          spelling,
          values: this.#convert(fn as GraphFunction),
        });
      }
    }
    this.#context = graph.imports.some((entry) => entry.contract === CONTEXT);
    this.#used = this.#exports.size > 0 || graph.imports.some((entry) => isKernel(entry.contract));
    const empty = new Map<never, never>();
    this.#revision = {
      contract: owner.contract,
      scopes: this.#scopes,
      // A component's Orb dependencies and reference views are served by its typed imports.
      views: empty,
      imports: empty,
      importMethods: empty,
      importSignatures: empty,
      hooks: this.#hooks,
      signatures: owner.signatures,
      execute: (command, execution, authority) => this.#execute(command, execution, authority),
    };
  }

  /**
   * Adopt the hooks the checked implementation found this Orb declaring, with their callback
   * signatures, and the import that runs each one (a hook without one is never run by its Orb).
   */
  declareHooks(hooks: readonly HookBinding[], signature: (hook: HookBinding) => Signature): void {
    for (const hook of hooks) {
      this.#hooks.set(hook.alias, hook);
      this.owner.signatures.set(hook.slot, signature(hook));
      const entry = this.owner.graph.imports.find((item) => item.contract === hook.name);
      if (entry) this.#runs.set(entry.binding, hook.alias);
    }
    if (hooks.length) this.#used = true;
  }

  /**
   * Declare one environment's kernel handles while its factory runs, as the native component
   * worker does: the root reads its address, binds the hooks this Orb runs and declares the
   * resources its component lists; a service object reads its address and declares the state its
   * component describes for the service interface `contract`.
   */
  async declare(
    environment: ComponentScope,
    declaration: Declaration,
    service: string | null,
    contract: string | undefined,
  ): Promise<void> {
    if (!this.#used) return;
    const scope = new Scope(this.#revision, declaration, service);
    this.#scopes.set(scope.id, scope);
    const root = service === null ? undefined : environment.parent?.kernel;
    if (service !== null && !root) throw new Error("Missing component Orb kernel table");
    const table = { scope, resources: new Map(), hooks: new Map(), context: this.#context } as {
      -readonly [Field in keyof KernelTable]: KernelTable[Field];
    };
    table.root = root ?? table;
    environment.kernel = table;
    const callback = environment.callback({ declaration });
    const { execution } = callback.kernel!;
    try {
      if (this.#context) table.address = execution.sync({ operation: "address" });
      if (service === null) {
        for (const [binding, alias] of this.#runs)
          table.hooks.set(
            binding,
            execution.sync({ operation: "hook.declare", name: alias }) as number,
          );
        const list = this.#exports.get(`${RESOURCES_INIT}#declare`);
        if (!list) return;
        const declared = await this.#call(environment, callback, list, []);
        if (!Array.isArray(declared)) throw new Error("The resource declarations are not a list");
        for (const resource of declared as { name?: unknown; version?: unknown }[]) {
          const { name, version } = resource;
          if (typeof name !== "string") throw new Error("A resource declaration carries no name");
          if (!isU32(version)) throw new Error("A resource declaration carries no version");
          const id = execution.sync({
            operation: "resource.declare",
            name,
            version,
            open: this.#number({ kind: "open", environment: environment.id, name }),
            close: this.#number({ kind: "close", environment: environment.id, name }),
            dependencies: [],
          });
          table.resources.set(name, id as number);
        }
        return;
      }
      const describe = this.#exports.get(`${STATE_INIT}#declare`);
      if (!describe || contract === undefined) return;
      const declared = (await this.#call(environment, callback, describe, [contract])) as {
        initial?: unknown;
        version?: unknown;
        validated?: unknown;
      } | null;
      if (declared === null) return;
      if (!isU32(declared.version)) throw new Error("The state declaration carries no version");
      if (typeof declared.validated !== "boolean")
        throw new Error("The state declaration does not say whether it is validated");
      table.state = execution.sync({
        operation: "state.declare",
        initial: json(declared.initial, "The initial state"),
        version: declared.version,
        // A validated state gets a number of its own; the kernel runs it, through
        // `validateEnvironment`, on the value a reload would hand to this instance.
        validate: declared.validated
          ? this.#number({ kind: "validate", environment: environment.id, service: contract })
          : null,
      }) as number;
    } finally {
      callback.cancel();
    }
  }

  /** Ask the component whether it accepts the retained state a reload would hand to it. */
  async validate(environment: ComponentScope, value: unknown): Promise<void> {
    const table = environment.kernel;
    if (!table) return;
    for (const number of table.scope.validators) {
      const numbered = this.#numbers.get(number);
      if (numbered?.kind !== "validate" || numbered.environment !== environment.id)
        throw new Error("Unknown compiled state validator");
      const validator = this.#exports.get(`${STATE_INIT}#validate`);
      if (!validator) throw new Error("The component declares no kernel state validator");
      // The validator may only read the retained state, as a source Orb's.
      const callback = environment.callback({ validation: true });
      let verdict: { tag?: unknown; val?: unknown };
      try {
        verdict = (await this.#call(environment, callback, validator, [
          numbered.service,
          encode(value).text,
        ])) as typeof verdict;
      } finally {
        callback.cancel();
      }
      if (verdict.tag === "err")
        throw new Error(
          typeof verdict.val === "string"
            ? `the state was refused: ${verdict.val}`
            : "the state was refused",
        );
      if (verdict.tag !== "ok") throw new Error("the state validator answered no result");
    }
  }

  /** Forget an environment's table and the numbers it issued once the kernel released it. */
  release(environment: number): void {
    this.#scopes.delete(environment);
    for (const [number, numbered] of this.#numbers)
      if (numbered.environment === environment) this.#numbers.delete(number);
  }

  /**
   * The import object of a kernel interface, or of the import that runs a hook this Orb
   * declares; built before instantiation, it resolves the hook when the guest calls it. Each
   * function serves the callback owning the guest's current root task.
   */
  imports(binding: string): Record<string, unknown> {
    const entry = this.owner.graph.imports.find((item) => item.binding === binding);
    if (!entry) throw new Error(`Browser component execution cannot provide import ${binding}`);
    const functions = Object.entries(entry.functions ?? {}).map(([name, fn]) => {
      const spelling = this.owner.graph.javascriptNames[name];
      if (spelling === undefined) throw new Error(`Missing WIT JavaScript name for ${name}`);
      return { name, spelling, values: this.#convert(fn as GraphFunction) };
    });
    const serve = (
      answer: (name: string, values: FunctionValues, args: unknown[]) => unknown,
    ): Record<string, unknown> =>
      Object.fromEntries(
        functions.map(({ name, spelling, values }) => [
          spelling,
          (...args: unknown[]) => answer(name, values, values.hostArguments(args)),
        ]),
      );
    switch (entry.contract) {
      case STATE:
        return serve((name, values, args) => values.toGuest(this.#state(name, args)));
      case RESOURCES:
        return serve((name, _, args) => this.#resource(name, args));
      case CONTEXT:
        return serve((name, values, args) => values.toGuest(this.#contextual(name, args)));
    }
    if (isKernel(entry.contract)) throw new Error(`The kernel serves no ${entry.contract}`);
    return serve((_, values, args) => this.#run(binding, values, args));
  }

  /** The current callback and its kernel access; a call between callbacks has neither. */
  #current() {
    const callback = this.owner.port.current();
    const access = callback.scope.kernel;
    if (!access) throw new Error("The kernel interfaces need a callback of this Orb");
    return { callback, ...access };
  }

  /** `state.read` and `state.update` of the state of the object the call runs on. */
  #state(name: string, args: readonly unknown[]): string {
    const { table, execution } = this.#current();
    if (table.state === undefined)
      throw new Error(
        table.root === table
          ? "contributions run in the Orb root, which owns no kernel state"
          : "this service object declares no kernel state",
      );
    let snapshot: unknown;
    if (name === "read" && args.length === 0)
      snapshot = execution.sync({ operation: "state.read", id: table.state });
    else if (name === "update" && args.length === 1) {
      const value = json(args[0], "The state value");
      try {
        snapshot = execution.sync({ operation: "state.update", id: table.state, value });
      } catch (error) {
        // The kernel keeps the previous value when it refuses the update of a cancelled call, and
        // `update` answers with the value the kernel retained, as it promises.
        if (execution.sync({ operation: "signal.cancelled" }) !== true) throw error;
        snapshot = execution.sync({ operation: "state.read", id: table.state });
      }
    } else throw new Error("unknown kernel state function");
    return JSON.stringify((snapshot as { value: unknown }).value);
  }

  /** `resource.read`: the handle the component's opener returned for a resource of its root. */
  #resource(name: string, args: readonly unknown[]): number {
    const { table, execution } = this.#current();
    const [resource] = args;
    if (name !== "read" || args.length !== 1 || typeof resource !== "string")
      throw new Error("unknown kernel resource function");
    const id = table.root.resources.get(resource);
    if (id === undefined) throw new Error(`the Orb root declares no resource named ${resource}`);
    const handle = execution.sync({ operation: "resource.read", scope: table.root.scope.id, id });
    if (!isU32(handle)) throw new Error("resource handle is not a u32");
    return handle;
  }

  /** The call's principal and cancellation, the object's address and the Orb's log. */
  #contextual(name: string, args: readonly unknown[]): unknown {
    const { table, execution, principal } = this.#current();
    switch (name) {
      case "object":
        if (table.address === undefined)
          throw new Error("the address of this environment's object was not declared");
        return table.address;
      case "principal":
        return principal ?? execution.sync({ operation: "context.principal" });
      case "cancelled":
        return execution.sync({ operation: "signal.cancelled" });
      case "cancel":
        try {
          execution.sync({ operation: "context.cancel" });
        } catch (error) {
          // Cancelling a call that is already cancelled changes nothing; the kernel refuses it.
          if (execution.sync({ operation: "signal.cancelled" }) !== true) throw error;
        }
        return null;
      case "log": {
        const [level, message] = args;
        if (typeof level !== "string" || !LEVELS.includes(level))
          throw new Error("Invalid console level");
        try {
          this.log?.({ orb: this.orb, level, message: String(message) });
        } catch {
          /* Logging observers do not control the guest. */
        }
        return null;
      }
    }
    throw new Error("unknown kernel context function");
  }

  /**
   * `hook.run` of a hook this Orb declares: the kernel runs the plan of contributions over the
   * single input, checked by the hook callback's WIT signature in both directions.
   */
  #run(binding: string, values: FunctionValues, args: unknown[]): Promise<unknown> {
    const { callback, table, execution } = this.#current();
    const alias = this.#runs.get(binding);
    const id = table.root.hooks.get(binding);
    if (alias === undefined || id === undefined)
      throw new Error(`the Orb root declares no hook for the import ${binding}`);
    if (args.length !== 1) throw new Error("a hook callback takes one input");
    const shape = this.owner.signatures.get(this.#hooks.get(alias)!.slot);
    const [value] = JSON.parse(encode(args, VALUE_LIMIT, shape?.params).text) as [unknown];
    const run = execution.async({
      operation: "hook.run",
      scope: table.root.scope.id,
      id,
      value,
    }) as Promise<Encoded>;
    return this.owner.port
      .physical(callback, run)
      .then((result) => values.toGuest(lift(shape?.result, JSON.parse(result.text))));
  }

  /** Issue one callback number; the numbers of a revision never repeat. */
  #number(numbered: Numbered): number {
    if (this.#next >= 0xffff_ffff) throw new Error("callback space exhausted");
    this.#numbers.set(++this.#next, numbered);
    return this.#next;
  }

  /** Run one kernel callback the component exports, admitted as `callback`. */
  #call(
    environment: ComponentScope,
    callback: CallbackScope,
    exported: Exported,
    args: readonly unknown[],
  ): Promise<unknown> {
    const port = this.owner.port;
    return port.run(BigInt(environment.id), callback, () =>
      exported.values.fromGuest(() => {
        const fn = port.exports?.[exported.binding]?.[exported.spelling];
        if (typeof fn !== "function")
          throw new Error(`The component does not export ${exported.binding}#${exported.spelling}`);
        return fn(...exported.values.guestArguments(args));
      }),
    );
  }

  /**
   * Open or close a root resource by the number this revision declared it with, under the
   * authority the kernel issued: a signal for an opener, an admitted root call for a closer.
   */
  async #execute(command: string, execution: ExecutionScope, authority: Authority) {
    const { callback: number, value } = JSON.parse(command) as {
      callback: number;
      value: unknown;
    };
    const numbered = this.#numbers.get(number);
    if (numbered?.kind !== "open" && numbered?.kind !== "close")
      throw new Error("Unknown compiled resource callback");
    const environment = this.owner.scopes.get(numbered.environment);
    if (!environment) throw new Error("The resource's Orb scope is gone");
    const exported = this.#exports.get(`${RESOURCES_INIT}#${numbered.kind}`);
    if (!exported) throw new Error("The component declares no kernel resource callbacks");
    const callback =
      "context" in authority
        ? environment.exchange(authority.context, execution)
        : environment.callback(authority, execution);
    const release = await this.owner.gate.enter(undefined, callback.signal);
    try {
      if (numbered.kind === "open") {
        const handle = await this.#call(environment, callback, exported, [numbered.name]);
        if (!isU32(handle)) throw new Error("resource handle is not a u32");
        return handle;
      }
      if (!isU32(value)) throw new Error("resource closer without a handle");
      await this.#call(environment, callback, exported, [numbered.name, value]);
      return null;
    } finally {
      callback.cancel();
      release();
    }
  }
}
