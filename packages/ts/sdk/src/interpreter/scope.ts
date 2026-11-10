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
  asyncSurvivesCancellation,
} from "./host-protocol.ts";
import type { Revision } from "./revision.ts";
import { check, encode } from "./codec.ts";

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
  readonly references = new Map<number, { reference: DynamicReference; binding: string }>();
  readonly resources = new Map<number, Resource<number>>();
  readonly hooks = new Map<
    number,
    { handle: ReturnType<Declaration["hook"]>; binding: import("./types.ts").HookBinding }
  >();
  #next = 0;

  /** Retain only issued declarations; service scopes inherit their own revision's Orb resources. */
  constructor(
    readonly revision: Revision,
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
        const id = ++this.#next;
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
        const id = ++this.#next;
        this.hooks.set(id, { handle, binding });
        return id;
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
        this.reference(request).reference.release();
        return null;
      case "reference.status":
        return this.reference(request).reference.status;
      case "reference.state": {
        const reference = this.reference(request).reference;
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
            this.revision.execute(
              JSON.stringify({ kind: "callback", callback: open, value: null }),
              this.exchange({ signal }),
            ) as Promise<number>,
          close: (value: number, context: Context) =>
            this.revision.execute(
              JSON.stringify({ kind: "callback", callback: close, value }),
              this.exchange({ context }),
            ),
        };
        const resource = declaration.resource<number>(request.name, options);
        const id = ++this.#next;
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

  /** Resolve a reference handle and its WIT binding from this table or the issued parent's. */
  private reference(request: { scope?: number; id: number }) {
    return this.handle(this.selected(request.scope).references, request.id, "reference");
  }

  /** Await only issued references and bounded cooperative timers. */
  private async async(
    request: AsyncHostRequest,
    authority: Authority,
    ended: AbortSignal,
  ): Promise<unknown> {
    checkSignal(ended);
    switch (request.operation) {
      case "transport.connect":
      case "transport.read":
      case "transport.write":
      case "transport.close":
      case "http.send":
      case "http.read":
      case "http.close": {
        if (!("context" in authority) && !("signal" in authority))
          throw new Error("I/O requires an active callback");
        // Closing an owned stream is cleanup; it still needs a callback but survives cancellation.
        if (!asyncSurvivesCancellation(request)) {
          if ("context" in authority) authority.context.check();
          else checkSignal(authority.signal);
        }
        // Cleanup may outlive the root's admission; inspecting its Context would refuse cleanup.
        const owner = asyncSurvivesCancellation(request) ? undefined : authoritySignal(authority);
        const ioSignal = owner ? combinedSignal(owner, ended) : ended;
        const result = await this.revision.io(request, ioSignal);
        if (request.operation === "transport.connect" || request.operation === "http.send") {
          // Cancellation ends the stream's authority; closing it settles pending socket work.
          const owner = ioSignal;
          const close = () =>
            void this.revision
              .io(
                {
                  operation: request.operation === "http.send" ? "http.close" : "transport.close",
                  id:
                    request.operation === "http.send"
                      ? (result as { id: number }).id
                      : (result as number),
                },
                ioSignal,
              )
              .catch(() => {});
          if (owner.aborted) close();
          else owner.addEventListener("abort", close, { once: true });
        }
        return result;
      }
      case "hook.run": {
        const context = this.context(authority);
        const { handle, binding } = this.handle(
          this.selected(request.scope).hooks,
          request.id,
          "hook",
        );
        check(this.revision.contract, binding.slot, true, encode([request.value]));
        const result = (await handle.run(context, request.value)) ?? null;
        return check(this.revision.contract, binding.slot, false, encode(result));
      }
      case "reference.ready": {
        const owner = authoritySignal(authority);
        await this.declaration.session.calls.observe(
          this.reference(request).reference.ready,
          owner ? combinedSignal(owner, ended) : ended,
        );
        return null;
      }
      case "reference.call": {
        const context = this.context(authority);
        const { reference, binding } = this.reference(request);
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
        const result =
          (await this.declaration.session.calls.invokePhysical(
            record,
            method,
            request.args,
            context,
            capture,
          )) ?? null;
        // The provider may be an ordinary host implementation returning hostile JavaScript. Reply
        // with the checked capture, never with the original object graph.
        return capture?.output ?? check(this.revision.contract, bound.slot, false, encode(result));
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
