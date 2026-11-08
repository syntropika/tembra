import type { Declaration } from "../authoring/declaration.ts";
import type { DynamicReference, Resource } from "../authoring/types.ts";
import type { Context } from "../runtime/context.ts";
import { resourceRealm } from "@syntropika/tembra-sdk/internal/adapters";
import type { State } from "../state.ts";
import type { Scope as ExchangeScope } from "./exchange.ts";
import type { Revision } from "./revision.ts";
import { kebab, query } from "./types.ts";

type Authority =
  | { declaration: Declaration }
  | { context: Context }
  | { signal: AbortSignal }
  | { validation: true };

/** Host-owned tables restrict guest numbers to one environment and its explicit Orb parent. */
export class Scope {
  readonly id: number;
  readonly realm: number;
  readonly parent: Scope | undefined;
  readonly validators: number[] = [];
  readonly states = new Map<number, State<unknown>>();
  readonly references = new Map<number, { reference: DynamicReference; binding: string }>();
  readonly resources = new Map<number, Resource<number>>();
  #next = 0;

  /** Retain only issued declarations; service scopes inherit their own revision's Orb resources. */
  constructor(
    readonly revision: Revision,
    readonly declaration: Declaration,
    readonly service: string | null,
  ) {
    this.id = declaration.environment.id;
    this.parent = service === null ? undefined : (declaration.orb as Scope);
    this.realm = this.parent?.realm ?? this.id;
  }

  /** Adapt an issued host authority to the interpreter transport without serializing it. */
  exchange(authority: Authority): ExchangeScope {
    const ended = new AbortController();
    return {
      cancel: () => ended.abort(),
      ...("context" in authority
        ? { signal: authority.context.signal }
        : "signal" in authority
          ? { signal: authority.signal }
          : {}),
      check: () => {
        if ("context" in authority) authority.context.check();
        if ("signal" in authority) authority.signal.throwIfAborted();
      },
      sync: (packet) => this.sync(JSON.parse(packet), authority),
      async: (packet) => this.async(JSON.parse(packet), authority, ended.signal),
    };
  }

  /** Require the actual admitted call for writes and dependent invocations. */
  private context(authority: Authority): Context {
    if (!("context" in authority)) throw new Error("Operation requires an admitted invocation");
    authority.context.check();
    return authority.context;
  }

  /** Reject declarations outside the original synchronous pass. */
  private factory(authority: Authority): Declaration {
    if (!("declaration" in authority)) throw new Error("Operation requires an active declaration");
    return authority.declaration;
  }

  /** Prevent a guessed scope identifier from selecting another object's handles. */
  private selected(request: any): Scope {
    if (request.scope === undefined || request.scope === this.id) return this;
    if (request.scope === this.parent?.id) return this.parent!;
    throw new Error("Foreign environment handle");
  }

  /** Resolve only handles issued from the selected host table. */
  private handle<T>(table: Map<number, T>, id: unknown): T {
    const value = typeof id === "number" ? table.get(id) : undefined;
    if (value === undefined) throw new Error("Unknown scope handle");
    return value;
  }

  /** Return the kernel's snapshot revision rather than inventing guest state identities. */
  private snapshot(value: unknown, object: number): unknown {
    const snapshot = this.declaration.session.snapshot(object);
    return { value, revision: snapshot.revision, version: snapshot.version };
  }

  /** Service synchronous requests against one immutable authority selected by the host callback. */
  private sync(request: any, authority: Authority): unknown {
    const selected = this.selected(request);
    const operation = request.operation;
    if ("validation" in authority && operation !== "state.read")
      throw new Error("Validators may only read retained state");
    if (
      ![
        "signal.cancelled",
        "state.read",
        "resource.read",
        "reference.state",
        "reference.status",
        "reference.release",
      ].includes(operation)
    ) {
      if ("context" in authority) authority.context.check();
      if ("signal" in authority) authority.signal.throwIfAborted();
    }
    switch (operation) {
      case "address": {
        const declaration = this.factory(authority);
        return {
          key: this.service === null ? [] : declaration.key,
          principal: declaration.principal,
        };
      }
      case "orb":
        this.factory(authority);
        return this.parent?.id ?? null;
      case "state.declare": {
        const declaration = this.factory(authority);
        const id = ++this.#next;
        const state = declaration.state(request.initial, { version: request.version });
        this.states.set(id, state);
        if (request.validate !== null) this.validators.push(request.validate);
        return id;
      }
      case "state.read":
        return selected.snapshot(
          this.handle(selected.states, request.id).value,
          selected.declaration.environment.object!.id,
        );
      case "state.update": {
        const state = this.handle(selected.states, request.id);
        const value = state.update(this.context(authority), () => request.value);
        return selected.snapshot(value, selected.declaration.environment.object!.id);
      }
      case "reference.declare": {
        const service = this.revision.imports.get(request.service);
        if (!service) throw new Error("Reference is not a selected WIT Orb import");
        const reference = this.factory(authority).get(service, request.key);
        const id = ++this.#next;
        this.references.set(id, { reference, binding: request.service });
        return id;
      }
      case "reference.release":
        this.handle(selected.references, request.id).reference.release();
        return null;
      case "reference.status":
        return this.handle(selected.references, request.id).reference.status;
      case "reference.state": {
        const reference = this.handle(selected.references, request.id).reference;
        const record = this.declaration.session.handles.reference(reference);
        return this.snapshot(reference.state.value, record.object);
      }
      case "resource.declare": {
        const declaration = this.factory(authority);
        const dependencies = request.dependencies.map(
          (dependency: { scope: number; id: number }) => {
            const owner = this.selected(dependency);
            return this.handle(owner.resources, dependency.id);
          },
        );
        const options = {
          version: request.version,
          deps: dependencies,
          [resourceRealm]: this.realm,
          open: (signal: AbortSignal) =>
            this.revision.execute(
              { kind: "callback", callback: request.open, value: null },
              this.exchange({ signal }),
            ) as Promise<number>,
          close: (value: number, context: Context) =>
            this.revision.execute(
              { kind: "callback", callback: request.close, value },
              this.exchange({ context }),
            ),
        };
        const resource = declaration.resource<number>(request.name, options);
        const id = ++this.#next;
        this.resources.set(id, resource);
        return id;
      }
      case "resource.read":
        return this.handle(selected.resources, request.id).value;
      case "context.check":
        this.context(authority);
        return null;
      case "context.cancel":
        this.context(authority).cancel();
        return null;
      case "context.principal":
        return this.context(authority).principal;
      case "filesystem":
      case "network":
        if (!("context" in authority || "signal" in authority))
          throw new Error("I/O requires an active callback");
        return null;
      case "signal.cancelled":
        return "context" in authority
          ? authority.context.signal.aborted
          : "signal" in authority
            ? authority.signal.aborted
            : false;
      default:
        throw new Error(`Unknown or forbidden host operation ${operation}`);
    }
  }

  /** Await only issued references and bounded cooperative timers. */
  private async async(request: any, authority: Authority, ended: AbortSignal): Promise<unknown> {
    const selected = this.selected(request);
    switch (request.operation) {
      case "reference.ready":
        await this.handle(selected.references, request.id).reference.ready;
        return null;
      case "reference.call": {
        const context = this.context(authority);
        const { reference, binding } = this.handle(selected.references, request.id);
        const method = kebab(request.method);
        query(this.revision.contract, {
          kind: "import-arguments",
          binding,
          method,
          value: request.args,
        });
        const record = this.declaration.session.handles.reference(reference);
        if (!record.methods.has(method)) throw new Error("Reference does not expose this method");
        const result =
          (await this.declaration.session.calls.invoke(record, method, request.args, context)) ??
          null;
        query(this.revision.contract, { kind: "import-result", binding, method, value: result });
        return result;
      }
      case "wait": {
        const ownerSignal =
          "context" in authority
            ? this.context(authority).signal
            : "signal" in authority
              ? authority.signal
              : undefined;
        const signal = ownerSignal ? AbortSignal.any([ownerSignal, ended]) : undefined;
        if (
          !signal ||
          !Number.isInteger(request.milliseconds) ||
          request.milliseconds < 0 ||
          request.milliseconds > 5000
        )
          throw new Error("Timer requires an active callback and at most 5000 milliseconds");
        signal.throwIfAborted();
        await new Promise<void>((resolve, reject) => {
          const cancel = () => {
            clearTimeout(timer);
            signal.removeEventListener("abort", cancel);
            reject(new Error("Operation cancelled"));
          };
          const timer = setTimeout(() => {
            signal.removeEventListener("abort", cancel);
            resolve();
          }, request.milliseconds);
          signal.addEventListener("abort", cancel, { once: true });
        });
        return null;
      }
      default:
        throw new Error("Unknown or forbidden asynchronous host operation");
    }
  }
}
