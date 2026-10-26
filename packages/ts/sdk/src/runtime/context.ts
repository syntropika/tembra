import { ContextKey, ContextKind, readContextValue } from "../context-key.ts";
import { KernelOp } from "../bridge/protocol.ts";
import { Errors } from "../errors.ts";

import type { AcquisitionOptions, DynamicReference } from "../authoring/types.ts";
import type { Invocation } from "../bridge/kernel.ts";
import { TembraError } from "../error.ts";
import type { Session } from "./session.ts";

const issued = new WeakMap<object, Context>();

/** An issued operation scope. Keeping this object does not extend its root or invocation. */
export class Context {
  /** Issue a context for an existing root and optional admitted call. */
  constructor(
    readonly session: Session,
    readonly root: number,
    readonly invocation?: Invocation,
    readonly frame: number = 0,
  ) {
    issued.set(this, this);
  }

  /** Derive one binding without changing identity or lifetime. */
  with<T>(key: ContextKey<T>, value: T): Context {
    const source = key.source(value, this);
    const frame = this.session.kernel.command<number>(KernelOp.ContextDerive, {
      root: this.root,
      context: this.frame,
      call: this.invocation?.id,
      key,
      ...(key.kind === ContextKind.Opaque ? { source } : { value }),
    });
    return new Context(this.session, this.root, this.invocation, frame);
  }

  /** Read the nearest binding of this typed key. */
  value<T>(key: ContextKey<T>): T | undefined {
    return readContextValue(this, key, this.frame);
  }

  /** Export only explicitly allowed scalar metadata. */
  exportValues(keys: readonly ContextKey<unknown>[]): Record<string, unknown> {
    const result: Record<string, unknown> = {};
    for (const key of keys) {
      if (key.kind === ContextKind.Opaque)
        throw new TypeError("Local opaque references cannot be exported");
      const value = this.value(key);
      if (value !== undefined) result[`${key.name}@${key.version}`] = value;
    }
    return result;
  }

  /** Read the root identity from the kernel. */
  get principal(): string {
    return this.session.kernel.command<string>(KernelOp.RootPrincipal, { root: this.root });
  }

  /** Read the shared cancellation signal for this root. */
  get signal(): AbortSignal {
    return this.session.calls.signal(this.root);
  }

  /** Reject a cancelled root or expired invocation. */
  check(): void {
    this.session.kernel.command(KernelOp.CheckContext, {
      root: this.root,
      call: this.invocation?.id,
    });
  }

  /** Cancel the root and dispatch its cancellation actions. */
  cancel(): void {
    this.session.kernel.command(KernelOp.CancelRoot, { root: this.root });
    this.session.lifecycle.flush();
  }

  /** Acquire a reference whose claim ends with this root. */
  get(
    service: string,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
  ): Promise<DynamicReference> {
    this.check();
    return this.session.handles.get(service, key, options, this);
  }

  /** Read a named host resource while the context remains valid. */
  resource<T>(name: string): import("../authoring/types.ts").Resource<T> {
    this.check();
    return this.session.scopes.hostResource<T>(name);
  }
}

/** Require a context issued by the same host without checking cancellation or expiry. */
export function issuedContext(value: unknown, session: Session): Context {
  if (typeof value !== "object" || value === null || issued.get(value)?.session !== session) {
    throw new TembraError(Errors.Forbidden.Context, "An issued context from this host is required");
  }
  return issued.get(value)!;
}

/** Require a live context issued by the same host. */
export function context(value: unknown, session: Session): Context {
  const result = issuedContext(value, session);
  result.check();
  return result;
}

/** Recognize an issued context without extending its lifetime. */
export function isContext(value: unknown): value is Context {
  return typeof value === "object" && value !== null && issued.has(value);
}
