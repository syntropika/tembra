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
import type { Handle } from "./host-protocol.ts";

/** The revision tables a scope resolves guest numbers against (interpreter or component). */
export type ScopeOwner = Pick<
  Revision,
  "hooks" | "imports" | "importMethods" | "importSignatures" | "signatures" | "contract" | "scopes"
> & {
  /** Run a registered resource `open` callback under the issued signal-only authority. */
  resourceOpen(environment: number, callback: number, scope: ExecutionScope): Promise<number>;
  /** Run a registered resource `close` callback under the closing context. */
  resourceClose(
    environment: number,
    callback: number,
    value: number,
    scope: ExecutionScope,
  ): Promise<unknown>;
};

const scopeEnded = Object.freeze(new Error("Interpreter callback scope ended"));

type Authority =
  | { declaration: Declaration }
  | { context: Context }
  | { signal: AbortSignal }
  | { validation: true };

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
    {
      handle: ReturnType<Declaration["hook"]>;
      binding: import("./types.ts").HookBinding;
    }
  >();
  /** A compiled component's default `main` reference per Orb import binding (root scope only). */
  readonly defaults = new Map<string, DynamicReference>();
  #next = 0;

  /** Retain only issued declarations; service scopes inherit their own revision's Orb resources. */
  constructor(
    readonly revision: ScopeOwner,
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
      checkSelection: (kind, handle) => {
        if (cancelled) throw scopeEnded;
        if (kind === "reference") this.reference(handle, authority);
        else this.handle(this.selected(handle.scope).hooks, handle.id, "hook");
      },
      call: (reference, binding, method, args) => {
        if (cancelled) return Promise.reject(scopeEnded);
        ended ??= new AbortController();
        return this.typedCall(reference, binding, method, args, authority, ended.signal);
      },
      hook: (hook, input, alias) => {
        if (cancelled) return Promise.reject(scopeEnded);
        ended ??= new AbortController();
        return this.typedHook(hook, input, alias, authority, ended.signal);
      },
    };
  }

  /**
   * Declare the default `main` handle of each Orb import for a compiled component, whose plain
   * import calls select no reference. Only the root factory declares them.
   */
  declareDefaults(declaration: Declaration): void {
    for (const [binding, service] of this.revision.imports)
      this.defaults.set(binding, declaration.get(service, ["main"]));
  }

  /** The Orb root scope of this environment. */
  private root(): Scope {
    return this.parent ?? this;
  }

  /**
   * A typed dependency call of a component. The guest selected `reference` in the same turn (a
   * selection, never authority) or, for compiled components, the root's default handle applies.
   * Arguments are checked under the import's own WIT signature before the provider sees them.
   */
  private async typedCall(
    selected: Handle | undefined,
    binding: string,
    method: string,
    args: readonly unknown[],
    authority: Authority,
    ended: AbortSignal,
  ): Promise<unknown> {
    checkSignal(ended);
    const context = this.context(authority);
    let reference: DynamicReference;
    if (selected) {
      const slot = this.reference(selected, authority);
      if (slot.binding !== binding)
        throw new Error("The selected reference belongs to another import");
      reference = slot.reference;
    } else {
      const fallback = this.root().defaults.get(binding);
      if (!fallback) throw new Error("A source Orb dependency call requires a selected reference");
      reference = fallback;
    }
    const bound = this.revision.importMethods.get(binding)?.get(method);
    if (!bound) throw new Error("Unknown prepared import method");
    const shape = this.revision.signatures.get(bound.slot);
    // Typed guest values are captured once, with WIT 64-bit positions read as bigint.
    const input = check(
      this.revision.contract,
      bound.slot,
      true,
      encode(args, VALUE_LIMIT, shape?.params),
    );
    const signature = this.revision.importSignatures.get(binding);
    const capture: CapturedInvocation | undefined = signature ? { signature, input } : undefined;
    const record = this.declaration.session.handles.reference(reference);
    if (!record.methods.has(bound.name)) throw new Error("Reference does not expose this method");
    const result =
      (await this.declaration.session.calls.invokePhysical(
        record,
        bound.name,
        lift(shape?.params, JSON.parse(input.text)) as unknown[],
        context,
        capture,
      )) ?? null;
    checkSignal(ended);
    const output =
      capture?.output ??
      check(this.revision.contract, bound.slot, false, encode(result, VALUE_LIMIT, shape?.result));
    return lift(shape?.result, JSON.parse(output.text));
  }

  /** Run a declared hook with a typed input; participants see checked host-domain values. */
  private async typedHook(
    selected: Handle,
    value: unknown,
    alias: string,
    authority: Authority,
    ended: AbortSignal,
  ): Promise<unknown> {
    checkSignal(ended);
    const context = this.context(authority);
    const { handle, binding } = this.handle(
      this.selected(selected.scope).hooks,
      selected.id,
      "hook",
    );
    if (binding.alias !== alias) throw new Error("The selected hook belongs to another interface");
    const shape = this.revision.signatures.get(binding.slot);
    const input = check(
      this.revision.contract,
      binding.slot,
      true,
      encode([value], VALUE_LIMIT, shape?.params),
    );
    const [lifted] = lift(shape?.params, JSON.parse(input.text)) as [unknown];
    const result = (await handle.run(context, lifted)) ?? null;
    checkSignal(ended);
    const output = check(
      this.revision.contract,
      binding.slot,
      false,
      encode(result, VALUE_LIMIT, shape?.result),
    );
    return lift(shape?.result, JSON.parse(output.text));
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
        const state = declaration.state(request.initial, {
          version: request.version,
        });
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
      case "reference.release":
        this.reference(request, authority).reference.release();
        return null;
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
        const options = {
          version: request.version,
          deps: dependencies,
          open: (signal: AbortSignal) =>
            this.revision.resourceOpen(this.id, open, this.exchange({ signal })),
          close: (value: number, context: Context) =>
            this.revision.resourceClose(this.id, close, value, this.exchange({ context })),
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
    if (selected !== this || current !== slot.root)
      throw new Error("Reference belongs to another root");
    if (!this.declaration.session.rootReferences.has(slot.root))
      throw new Error("Reference expired with its root");
    return slot;
  }

  /**
   * `Context.get`: acquire a selected WIT Orb import for the active invocation. The host resolves
   * the provider and the installed requirement's API version and methods; the guest names only the
   * binding and key, and the root's principal applies. Readiness is awaited under the callback's
   * cancellation, and a claim whose wait or authority fails is released before any handle exists.
   */
  private async acquire(
    request: { service: string; key: readonly string[] },
    authority: Authority,
    ended: AbortSignal,
  ): Promise<number> {
    const context = this.context(authority);
    // Only an admitted method call selects instances. A cleanup context (a resource `close`) has
    // no invocation; acquiring there would choose instances by the cleanup principal.
    if (!context.invocation)
      throw new Error("Reference acquisition requires an admitted invocation");
    const binding = this.revision.imports.get(request.service);
    if (!binding) throw new Error("Reference is not a selected WIT Orb import");
    const session = this.declaration.session;
    const requirement = session.scopes.requirement(this.declaration.environment, binding);
    const reference = session.handles.hold(
      requirement.service,
      request.key,
      {
        ...(requirement.apiVersion ? { apiVersion: requirement.apiVersion } : {}),
        ...(requirement.methods ? { methods: requirement.methods } : {}),
      },
      context,
    );
    let id: number;
    try {
      await session.calls.observe(reference.ready, combinedSignal(context.signal, ended));
      // The invocation, root or callback may have ended while readiness was pending.
      context.check();
      checkSignal(ended);
      id = this.allocate();
    } catch (error) {
      this.abandon(reference, error);
    }
    // Root claims are released by the runtime; forget the names of those whose roots have ended.
    for (const [handle, slot] of this.references)
      if (slot.root !== undefined && !session.rootReferences.has(slot.root))
        this.references.delete(handle);
    this.references.set(id, {
      reference,
      binding: request.service,
      root: context.root,
    });
    return id;
  }

  /**
   * Release a claim that will never be published and rethrow the failure that stopped it; a
   * secondary release failure is reported to the session instead of replacing it.
   */
  private abandon(reference: DynamicReference, error: unknown): never {
    try {
      reference.release();
    } catch (release) {
      this.declaration.session.report(release);
    }
    throw error;
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
        if (request.binding !== binding)
          throw new Error("The selected reference belongs to another import");
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
