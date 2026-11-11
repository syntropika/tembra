/**
 * Host authority of one compiled component environment.
 *
 * A compiled component has no Tembra control imports: it cannot declare state, hooks, resources
 * or references. Its only host authority beyond granted WASI is calling its typed Orb dependency
 * imports. A plain import targets the default `main` reference the root factory declared for its
 * binding. A checked reference view (an artifact role) declares no default: the guest acquires
 * root claims by key through the view's acquisition function and calls the provider through the
 * returned host-local handle. Every call runs under the admitted invocation of the callback that
 * issued it, with arguments and results checked by the import's own WIT signature.
 */
import type { CapturedInvocation } from "#tembra-adapters";
import type { Declaration } from "../../authoring/declaration.ts";
import type { DynamicReference } from "../../authoring/types.ts";
import type { Context } from "../../runtime/context.ts";
import { checkSignal, combinedSignal } from "../../runtime/signals.ts";
import { acquireReference } from "../acquire.ts";
import { check, encode, VALUE_LIMIT } from "../codec.ts";
import type { ContractPort } from "../types.ts";
import { lift, type Signature } from "../wit-shape.ts";

const scopeEnded = Object.freeze(new Error("Component callback scope ended"));

/** One prepared import function: the provider method it calls and its original WIT slot. */
export interface BoundMethod {
  readonly name: string;
  readonly slot: number;
}

/**
 * An opaque, unforgeable token naming one ready root claim of a reference view. Only the scope
 * that issued it can resolve it, and only for a callback of the root owning the claim.
 */
export interface ReferenceClaim {
  readonly binding: string;
}

interface Claim {
  readonly owner: ScopeOwner;
  readonly binding: string;
  readonly reference: DynamicReference;
  readonly root: number;
}

/** Issued claims; a token absent from this map was not issued by any component scope. */
const claims = new WeakMap<ReferenceClaim, Claim>();

/** The callback authority a component port serves host imports with. */
export interface CallbackScope {
  readonly signal?: AbortSignal;
  check(): void;
  /** Revoke the callback: later calls are refused and pending waits stop. */
  cancel(): void;
  /**
   * Call a plain Orb dependency import with host-domain arguments (WIT 64-bit positions as
   * `bigint`) and answer its checked host-domain result.
   */
  call(binding: string, method: string, args: readonly unknown[]): Promise<unknown>;
  /**
   * Acquire a reference view's provider at `key` for this callback's admitted invocation and
   * root. `abandoned` stops a pending readiness wait and releases the claim.
   */
  acquire(binding: string, key: readonly string[], abandoned: AbortSignal): Promise<ReferenceClaim>;
  /** Call view `method` (its original WIT name) on the claim of exactly `binding`. */
  invoke(
    claim: ReferenceClaim,
    binding: string,
    method: string,
    args: readonly unknown[],
  ): Promise<unknown>;
  /**
   * Release a claim of this callback's root. A claim of another root is left to its root, which
   * releases it when it ends; a drop never acts for another root.
   */
  release(claim: ReferenceClaim): void;
}

/** The revision tables a component scope resolves import calls against. */
export interface ScopeOwner {
  readonly contract: ContractPort;
  readonly imports: ReadonlyMap<string, string>;
  /** Plain import methods by JavaScript spelling. */
  readonly importMethods: ReadonlyMap<string, ReadonlyMap<string, BoundMethod>>;
  /** Reference-view methods by original WIT name (`[method]R.name`). */
  readonly views: ReadonlyMap<string, ReadonlyMap<string, BoundMethod>>;
  readonly importSignatures: ReadonlyMap<string, string>;
  readonly signatures: ReadonlyMap<number, Signature>;
  readonly scopes: ReadonlyMap<number, ComponentScope>;
}

/** One kernel environment (the Orb root or a service instance) of a compiled component. */
export class ComponentScope {
  readonly id: number;
  readonly parent: ComponentScope | undefined;
  /** Default `main` reference per plain Orb import binding (root scope only). */
  readonly defaults = new Map<string, DynamicReference>();

  constructor(
    readonly owner: ScopeOwner,
    readonly declaration: Declaration,
    service: string | null,
  ) {
    this.id = declaration.environment.id;
    const parent = service === null ? undefined : declaration.environment.orb.scope;
    this.parent = parent ? owner.scopes.get(parent.id) : undefined;
    if (service !== null && !this.parent) throw new Error("Missing component Orb scope");
    // Only the root factory declares the default dependency handles, once. A reference view
    // selects its objects by key and has no default.
    if (service === null)
      for (const [binding, name] of owner.imports)
        if (!owner.views.has(binding)) this.defaults.set(binding, declaration.get(name, ["main"]));
  }

  /** Adapt one admitted invocation to the component port without exposing it to the guest. */
  exchange(context: Context): CallbackScope {
    const ended = new AbortController();
    // Match native guest drops: a long callback can drop and reacquire the same address
    // without prematurely retiring its provider. Root cancellation remains immediate.
    const dropped = new Set<ReferenceClaim>();
    const release = (token: ReferenceClaim) => {
      const claim = this.claim(token, token.binding);
      if (claim.root === context.root) claim.reference.release();
    };
    const live = () => {
      if (ended.signal.aborted) throw scopeEnded;
    };
    return {
      signal: context.signal,
      check: () => context.check(),
      cancel: () => {
        if (!ended.signal.aborted) ended.abort(scopeEnded);
        for (const token of dropped) {
          try {
            release(token);
          } catch (error) {
            this.declaration.session.report(error);
          }
        }
        dropped.clear();
      },
      call: async (binding, method, args) => {
        live();
        const reference = (this.parent ?? this).defaults.get(binding);
        if (!reference) throw new Error("The component has no Orb dependency for this import");
        const bound = this.owner.importMethods.get(binding)?.get(method);
        if (!bound) throw new Error("Unknown prepared import method");
        return this.call(reference, binding, bound, args, context, ended.signal);
      },
      acquire: async (binding, key, abandoned) => {
        live();
        return this.acquire(binding, key, context, combinedSignal(ended.signal, abandoned));
      },
      invoke: async (token, binding, method, args) => {
        live();
        const claim = this.claim(token, binding);
        if (claim.root !== context.root) throw new Error("Reference belongs to another root");
        const session = this.declaration.session;
        if (!session.rootReferences.has(claim.root))
          throw new Error("Reference expired with its root");
        if (session.handles.reference(claim.reference).released)
          throw new Error("Reference was released");
        const bound = this.owner.views.get(binding)?.get(method);
        if (!bound) throw new Error("Unknown prepared reference method");
        return this.call(claim.reference, binding, bound, args, context, ended.signal);
      },
      release: (token) => {
        const claim = this.claim(token, token.binding);
        if (claim.root !== context.root) return;
        if (ended.signal.aborted) release(token);
        else dropped.add(token);
      },
    };
  }

  /** Resolve a token this revision issued for exactly `binding`. */
  private claim(token: ReferenceClaim, binding: string): Claim {
    const claim = claims.get(token);
    if (!claim || claim.owner !== this.owner || claim.binding !== binding)
      throw new Error("Foreign reference handle");
    return claim;
  }

  /** Acquire a ready root claim and issue its opaque token. */
  private async acquire(
    binding: string,
    key: readonly string[],
    context: Context,
    ended: AbortSignal,
  ): Promise<ReferenceClaim> {
    if (!this.owner.views.has(binding)) throw new Error("The import is not a reference view");
    const service = this.owner.imports.get(binding);
    if (!service) throw new Error("Reference is not a selected WIT Orb import");
    return acquireReference(this.declaration, service, key, context, ended, (reference) => {
      const token: ReferenceClaim = Object.freeze({ binding });
      claims.set(token, { owner: this.owner, binding, reference, root: context.root });
      return token;
    });
  }

  /** Arguments are checked under the import's own WIT slot before the provider sees them. */
  private async call(
    reference: DynamicReference,
    binding: string,
    bound: BoundMethod,
    args: readonly unknown[],
    context: Context,
    ended: AbortSignal,
  ): Promise<unknown> {
    checkSignal(ended);
    context.check();
    const shape = this.owner.signatures.get(bound.slot);
    // Typed guest values are captured once, with WIT 64-bit positions read as bigint.
    const input = check(
      this.owner.contract,
      bound.slot,
      true,
      encode(args, VALUE_LIMIT, shape?.params),
    );
    const signature = this.owner.importSignatures.get(binding);
    const capture: CapturedInvocation | undefined = signature ? { signature, input } : undefined;
    const session = this.declaration.session;
    const record = session.handles.reference(reference);
    if (!record.methods.has(bound.name)) throw new Error("Reference does not expose this method");
    const result =
      (await session.calls.invokePhysical(
        record,
        bound.name,
        lift(shape?.params, JSON.parse(input.text)) as unknown[],
        context,
        capture,
      )) ?? null;
    checkSignal(ended);
    // The provider may be an ordinary host implementation returning hostile JavaScript: reply
    // with the checked capture or a checked copy, never with the original object graph.
    const output =
      capture?.output ??
      check(this.owner.contract, bound.slot, false, encode(result, VALUE_LIMIT, shape?.result));
    return lift(shape?.result, JSON.parse(output.text));
  }
}
