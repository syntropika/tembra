/**
 * Host authority of one compiled component environment.
 *
 * A compiled component has no Tembra control imports: it cannot declare state, hooks, resources
 * or references, and it selects no dependency handle. Its only host authority beyond granted WASI
 * is calling its typed Orb dependency imports, which target the default `main` reference the root
 * factory declared for each import binding. Every call runs under the admitted invocation of the
 * callback that issued it, with arguments and results checked by the import's own WIT signature.
 */
import type { CapturedInvocation } from "#tembra-adapters";
import type { Declaration } from "../../authoring/declaration.ts";
import type { DynamicReference } from "../../authoring/types.ts";
import type { Context } from "../../runtime/context.ts";
import { checkSignal } from "../../runtime/signals.ts";
import { check, encode, VALUE_LIMIT } from "../codec.ts";
import type { ContractPort } from "../types.ts";
import { lift, type Signature } from "../wit-shape.ts";

const scopeEnded = Object.freeze(new Error("Component callback scope ended"));

/** The callback authority a component port serves host imports with. */
export interface CallbackScope {
  readonly signal?: AbortSignal;
  check(): void;
  /** Revoke the callback: later calls are refused and pending waits stop. */
  cancel(): void;
  /**
   * Call an Orb dependency import with host-domain arguments (WIT 64-bit positions as `bigint`)
   * and answer its checked host-domain result.
   */
  call(binding: string, method: string, args: readonly unknown[]): Promise<unknown>;
}

/** The revision tables a component scope resolves import calls against. */
export interface ScopeOwner {
  readonly contract: ContractPort;
  readonly imports: ReadonlyMap<string, string>;
  readonly importMethods: ReadonlyMap<string, ReadonlyMap<string, { name: string; slot: number }>>;
  readonly importSignatures: ReadonlyMap<string, string>;
  readonly signatures: ReadonlyMap<number, Signature>;
  readonly scopes: ReadonlyMap<number, ComponentScope>;
}

/** One kernel environment (the Orb root or a service instance) of a compiled component. */
export class ComponentScope {
  readonly id: number;
  readonly parent: ComponentScope | undefined;
  /** Default `main` reference per Orb import binding (root scope only). */
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
    // Only the root factory declares the default dependency handles, once.
    if (service === null)
      for (const [binding, name] of owner.imports)
        this.defaults.set(binding, declaration.get(name, ["main"]));
  }

  /** Adapt one admitted invocation to the component port without exposing it to the guest. */
  exchange(context: Context): CallbackScope {
    const ended = new AbortController();
    return {
      signal: context.signal,
      check: () => context.check(),
      cancel: () => {
        if (!ended.signal.aborted) ended.abort(scopeEnded);
      },
      call: (binding, method, args) => {
        if (ended.signal.aborted) return Promise.reject(scopeEnded);
        return this.call(binding, method, args, context, ended.signal);
      },
    };
  }

  /** Arguments are checked under the import's own WIT signature before the provider sees them. */
  private async call(
    binding: string,
    method: string,
    args: readonly unknown[],
    context: Context,
    ended: AbortSignal,
  ): Promise<unknown> {
    checkSignal(ended);
    context.check();
    const reference = (this.parent ?? this).defaults.get(binding);
    if (!reference) throw new Error("The component has no Orb dependency for this import");
    const bound = this.owner.importMethods.get(binding)?.get(method);
    if (!bound) throw new Error("Unknown prepared import method");
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
