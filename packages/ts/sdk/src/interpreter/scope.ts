import { checkSignal, combinedSignal } from "../runtime/signals.ts";
import type { CapturedInvocation } from "#tembra-adapters";
import type { Declaration } from "../authoring/declaration.ts";
import type { DynamicReference, Resource } from "../authoring/types.ts";
import type { Context } from "../runtime/context.ts";
import type { State } from "../state.ts";
import type { ExecutionScope } from "./types.ts";
import {
  type AsyncHostRequest,
  type HostRequest,
  WAIT_LIMIT_MILLISECONDS,
  survivesCancellation,
} from "./host-protocol.ts";
import type { Revision } from "./revision.ts";
import { check, encode, VALUE_LIMIT } from "./codec.ts";
import { lift } from "./wit-shape.ts";
import { acquireReference } from "./acquire.ts";

const scopeEnded = Object.freeze(new Error("Interpreter callback scope ended"));

/** The host-issued authority one callback's requests are served under. */
export type Authority =
  | { declaration: Declaration }
  | { context: Context }
  | { signal: AbortSignal }
  | { validation: true };

/**
 * The revision tables and callback executor a scope serves requests with. A source revision is
 * one; a prepared browser component lends its own for the kernel interfaces its Orb imports
 * (`component/kernel.ts`), so both answer the same requests with this same code.
 */
export type ScopeRevision = Pick<
  Revision,
  | "contract"
  | "scopes"
  | "views"
  | "imports"
  | "importMethods"
  | "importSignatures"
  | "hooks"
  | "signatures"
> & {
  /**
   * Run a resource callback the guest numbered with the scope its `authority` issued. A source
   * revision needs only the scope; a prepared component also admits its dependency calls with
   * the authority itself, which stays on the host.
   */
  execute(command: string, scope: ExecutionScope, authority: Authority): Promise<unknown>;
};

/** Read cancellation from the host-issued authority without transferring it to the guest. */
function authoritySignal(authority: Authority): AbortSignal | undefined {
  if ("context" in authority) return authority.context.signal;
  if ("signal" in authority) return authority.signal;
  return undefined;
}

/** Host-owned tables restrict guest numbers to one environment and its explicit Orb parent. */
export class Scope {
  readonly id: number;
  readonly parent: Scope | undefined;
  readonly validators: number[] = [];
  readonly states = new Map<number, State<unknown>>();
  /**
   * Declared references belong to this environment. A `root` marks a `Context.get` handle: the
   * runtime's root claim list owns it and releases it when that root ends.
   */
  readonly references = new Map<
    number,
    { reference: DynamicReference; binding: string; root?: number }
  >();
  readonly resources = new Map<number, Resource<number>>();
  readonly hooks = new Map<
    number,
    { handle: ReturnType<Declaration["hook"]>; binding: import("./types.ts").HookBinding }
  >();
  #next = 0;

  /** Retain only issued declarations; service scopes inherit their own revision's Orb resources. */
  constructor(
    readonly revision: ScopeRevision,
    readonly declaration: Declaration,
    readonly service: string | null,
  ) {
    this.id = declaration.environment.id;
    const parent = service === null ? undefined : declaration.environment.orb.scope;
    this.parent = service === null || !parent ? undefined : revision.scopes.get(parent.id);
    if (service !== null && !this.parent) throw new Error("Missing interpreter Orb scope");
  }

  /** Adapt an issued host authority to the interpreter transport without serializing it. */
  exchange(authority: Authority): ExecutionScope {
    let ended: AbortController | undefined;
    let cancelled = false;
    const signal = authoritySignal(authority);
    return {
      cancel: () => {
        if (cancelled) return;
        cancelled = true;
        ended?.abort(scopeEnded);
      },
      ...(signal ? { signal } : {}),
      check: () => {
        if ("context" in authority) authority.context.check();
        if ("signal" in authority) checkSignal(authority.signal);
      },
      sync: (request) => this.sync(request, authority),
      async: (request) => {
        if (cancelled) return Promise.reject(scopeEnded);
        ended ??= new AbortController();
        return this.async(request, authority, ended.signal);
      },
      deliver: (request, value, publish) => {
        let accepted = false;
        try {
          accepted = publish(value);
        } finally {
          // The worker may cancel an import while this host is awaiting readiness. Never
          // leave a ready but undelivered view owned by an otherwise long-lived root.
          if (
            !accepted &&
            request.operation === "reference.acquire" &&
            this.revision.views.has(request.service) &&
            typeof value === "number"
          ) {
            const table = this.parent ?? this;
            const slot = table.references.get(value);
            const root = "context" in authority ? authority.context.root : undefined;
            if (slot?.binding === request.service && slot.root === root) {
              table.references.delete(value);
              slot.reference.release();
            }
          }
        }
      },
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

  /** Select this table or the issued parent's; a scope is a table selector, never an authority. */
  private selected(scope: number | undefined): Scope {
    if (scope === undefined || scope === this.id) return this;
    if (this.parent !== undefined && scope === this.parent.id) return this.parent;
    throw new Error("Foreign environment handle");
  }

  /** Resolve only handles issued from the selected host table. */
  private handle<T>(table: Map<number, T>, id: number, kind: string): T {
    const value = table.get(id);
    if (value === undefined) throw new Error(`Unknown ${kind} handle`);
    return value;
  }

  /** Match the native u32 handle space and never reuse an expired handle's identity. */
  private allocate(): number {
    if (this.#next >= 0xffff_ffff) throw new Error("Scope handle space exhausted");
    return ++this.#next;
  }

  /**
   * Return the kernel's snapshot revision rather than inventing guest state identities. When the
   * guest already retains the current revision, only the identity crosses back; the protected value
   * is omitted because a revision never names two different values.
   */
  private snapshot(value: unknown, object: number, known?: string): unknown {
    const snapshot = this.declaration.session.snapshot(object);
    return snapshot.revision === known
      ? { revision: snapshot.revision, version: snapshot.version }
      : { value, revision: snapshot.revision, version: snapshot.version };
  }

  /** Service synchronous requests against one immutable authority selected by the host callback. */
  private sync(request: HostRequest, authority: Authority): unknown {
    if (request.operation === "console")
      throw new Error("Console records are consumed by the interpreter port");
    if ("validation" in authority && request.operation !== "state.read")
      throw new Error("Validators may only read retained state");
    if (!survivesCancellation(request)) {
      if ("context" in authority) authority.context.check();
      if ("signal" in authority) checkSignal(authority.signal);
    }
    switch (request.operation) {
      case "address": {
        const declaration = this.factory(authority);
        return {
          key: this.service === null ? [] : declaration.key,
          principal: declaration.principal,
        };
      }
      case "orb":
        this.factory(authority);
        // The root Orb environment has no parent; the guest observes `undefined`.
        return this.parent?.id ?? null;
      case "state.declare": {
        const declaration = this.factory(authority);
        const id = this.allocate();
        const state = declaration.state(request.initial, { version: request.version });
        this.states.set(id, state);
        if (request.validate !== null) this.validators.push(request.validate);
        return id;
      }
      case "state.read": {
        // Ownership and the state's lifetime are checked even when its value is not resent.
        const selected = this.selected(request.scope);
        return selected.snapshot(
          this.handle(selected.states, request.id, "state").value,
          selected.declaration.environment.object!.id,
          request.known,
        );
      }
      case "state.update": {
        const selected = this.selected(request.scope);
        const state = this.handle(selected.states, request.id, "state");
        const value = state.update(this.context(authority), () => request.value);
        return selected.snapshot(value, selected.declaration.environment.object!.id);
      }
      case "hook.declare": {
        const binding = this.revision.hooks.get(request.name);
        if (!binding) throw new Error("Hook is absent from the checked WIT declaration");
        const handle = this.factory(authority).hook(binding.name);
        const id = this.allocate();
        this.hooks.set(id, { handle, binding });
        return id;
      }
      case "reference.declare": {
        const service = this.revision.imports.get(request.service);
        if (!service) throw new Error("Reference is not a selected WIT Orb import");
        const reference = this.factory(authority).get(service, request.key);
        const id = this.allocate();
        this.references.set(id, { reference, binding: request.service });
        return id;
      }
      case "reference.release": {
        const slot = this.reference(request, authority);
        slot.reference.release();
        if (this.revision.views.has(slot.binding))
          this.selected(request.scope).references.delete(request.id);
        return null;
      }
      case "reference.status":
        return this.reference(request, authority).reference.status;
      case "reference.state": {
        const reference = this.reference(request, authority).reference;
        const record = this.declaration.session.handles.reference(reference);
        return this.snapshot(reference.state.value, record.object, request.known);
      }
      case "resource.declare": {
        const declaration = this.factory(authority);
        const dependencies = request.dependencies.map((dependency) =>
          this.handle(
            this.selected(dependency.scope).resources,
            dependency.id,
            "resource dependency",
          ),
        );
        const { open, close } = request;
        const run = (callback: number, value: number | null, authority: Authority) =>
          this.revision.execute(
            JSON.stringify({ kind: "callback", callback, value }),
            this.exchange(authority),
            authority,
          );
        const options = {
          version: request.version,
          deps: dependencies,
          open: (signal: AbortSignal) => run(open, null, { signal }) as Promise<number>,
          close: (value: number, context: Context) => run(close, value, { context }),
        };
        const resource = declaration.resource<number>(request.name, options);
        const id = this.allocate();
        this.resources.set(id, resource);
        return id;
      }
      case "resource.read":
        return this.handle(this.selected(request.scope).resources, request.id, "resource").value;
      case "context.check":
        if (!("context" in authority)) throw new Error("Operation requires an admitted invocation");
        return null;
      case "context.cancel":
        this.context(authority).cancel();
        return null;
      case "context.principal":
        return this.context(authority).principal;
      case "signal.cancelled":
        return authoritySignal(authority)?.aborted ?? false;
    }
  }

  /**
   * Resolve a reference handle and its WIT binding for the current callback. Declared handles come
   * from this table or the issued parent's. A `Context.get` handle resolves only in the table that
   * issued it, for a callback of the root owning its claim, and only while that root lasts.
   */
  private reference(request: { scope?: number; id: number }, authority: Authority) {
    const selected = this.selected(request.scope);
    const slot = this.handle(selected.references, request.id, "reference");
    if (slot.root === undefined) return slot;
    const current = "context" in authority ? authority.context.root : undefined;
    const issued =
      selected === this || (this.revision.views.has(slot.binding) && selected === this.parent);
    if (!issued || current !== slot.root) throw new Error("Reference belongs to another root");
    if (!this.declaration.session.rootReferences.has(slot.root))
      throw new Error("Reference expired with its root");
    return slot;
  }

  /**
   * `Context.get`: acquire a selected WIT Orb import for the active invocation (see
   * `acquire.ts`). Only a ready claim receives a handle, filed with the root owning it.
   */
  private async acquire(
    request: { service: string; key: readonly string[] },
    authority: Authority,
    ended: AbortSignal,
  ): Promise<number> {
    const context = this.context(authority);
    const binding = this.revision.imports.get(request.service);
    if (!binding) throw new Error("Reference is not a selected WIT Orb import");
    const session = this.declaration.session;
    const table = this.revision.views.has(request.service) ? (this.parent ?? this) : this;
    const { id, reference } = await acquireReference(
      this.declaration,
      binding,
      request.key,
      context,
      ended,
      (reference) => ({ id: table.allocate(), reference }),
    );
    // Root claims are released by the runtime; forget the names of those whose roots have ended.
    for (const [handle, slot] of table.references)
      if (slot.root !== undefined && !session.rootReferences.has(slot.root))
        table.references.delete(handle);
    table.references.set(id, { reference, binding: request.service, root: context.root });
    return id;
  }

  /** Await only issued references and bounded cooperative timers. */
  private async async(
    request: AsyncHostRequest,
    authority: Authority,
    ended: AbortSignal,
  ): Promise<unknown> {
    checkSignal(ended);
    switch (request.operation) {
      case "hook.run": {
        const context = this.context(authority);
        const { handle, binding } = this.handle(
          this.selected(request.scope).hooks,
          request.id,
          "hook",
        );
        // The guest's wire text is checked as is; host participants then receive bigint values.
        check(this.revision.contract, binding.slot, true, encode([request.value]));
        const shape = this.revision.signatures.get(binding.slot);
        const [input] = lift(shape?.params, [request.value]) as [unknown];
        const result = (await handle.run(context, input)) ?? null;
        return check(
          this.revision.contract,
          binding.slot,
          false,
          encode(result, VALUE_LIMIT, shape?.result),
        );
      }
      case "reference.ready": {
        const owner = authoritySignal(authority);
        await this.declaration.session.calls.observe(
          this.reference(request, authority).reference.ready,
          owner ? combinedSignal(owner, ended) : ended,
        );
        return null;
      }
      case "reference.acquire":
        return this.acquire(request, authority, ended);
      case "reference.call": {
        const context = this.context(authority);
        const { reference, binding } = this.reference(request, authority);
        const bound = this.revision.importMethods.get(binding)?.get(request.method);
        if (!bound) throw new Error("Unknown prepared import method");
        const method = bound.name;
        // The importing heap is untrusted: check its decoded arguments under its own signature.
        const input = check(this.revision.contract, bound.slot, true, encode(request.args));
        const signature = this.revision.importSignatures.get(binding);
        const capture: CapturedInvocation | undefined = signature
          ? { signature, input }
          : undefined;
        // The WIT check above already rejects non-list arguments; this narrows the payload.
        if (!Array.isArray(request.args)) throw new Error("Import arguments must be a list");
        const record = this.declaration.session.handles.reference(reference);
        if (!record.methods.has(method)) throw new Error("Reference does not expose this method");
        // Whether the captured route runs is decided by the provider at invocation, not by the
        // existence of a signature. Every non-captured route (a native adapter or a host handler)
        // takes the JavaScript domain form, WIT 64-bit positions as bigint; the captured route
        // ignores these arguments and splices the checked wire text `input` instead.
        const shape = this.revision.signatures.get(bound.slot);
        const result =
          (await this.declaration.session.calls.invokePhysical(
            record,
            method,
            lift(shape?.params, request.args) as unknown[],
            context,
            capture,
          )) ?? null;
        // The provider may be an ordinary host implementation returning hostile JavaScript. Reply
        // with the checked capture, never with the original object graph.
        return (
          capture?.output ??
          check(
            this.revision.contract,
            bound.slot,
            false,
            encode(result, VALUE_LIMIT, shape?.result),
          )
        );
      }
      case "wait": {
        const { milliseconds } = request;
        if (milliseconds > WAIT_LIMIT_MILLISECONDS)
          throw new Error("Timer exceeds the callback wait limit");
        if ("context" in authority) this.context(authority);
        const ownerSignal = authoritySignal(authority);
        if (!ownerSignal) throw new Error("Timer requires an active callback");
        const signal = combinedSignal(ownerSignal, ended);
        checkSignal(signal);
        await new Promise<void>((resolve, reject) => {
          const cancel = () => {
            clearTimeout(timer);
            signal.removeEventListener("abort", cancel);
            reject(new Error("Operation cancelled"));
          };
          const timer = setTimeout(() => {
            signal.removeEventListener("abort", cancel);
            resolve();
          }, milliseconds);
          signal.addEventListener("abort", cancel, { once: true });
        });
        return null;
      }
    }
  }
}
