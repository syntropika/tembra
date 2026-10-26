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
  ) {
    issued.set(this, this);
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

/** Require a live context issued by the same host. */
export function context(value: unknown, session: Session): Context {
  if (typeof value !== "object" || value === null || issued.get(value)?.session !== session) {
    throw new TembraError(Errors.Forbidden.Context, "An issued context from this host is required");
  }
  const result = issued.get(value)!;
  result.check();
  return result;
}

/** Recognize an issued context without extending its lifetime. */
export function isContext(value: unknown): value is Context {
  return typeof value === "object" && value !== null && issued.has(value);
}
