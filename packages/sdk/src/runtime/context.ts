import { Errors } from "../errors.ts";
import { KernelOp } from "../protocol.ts";

import { TembraError } from "../error.ts";
import type { Invocation } from "../kernel.ts";
import type { AcquisitionOptions, DynamicReference } from "../types.ts";
import type { Session } from "./session.ts";

const issued = new WeakMap<object, Context>();

/** An issued operation scope. Keeping this object does not extend its root or invocation. */
export class Context {
  constructor(
    readonly session: Session,
    readonly root: number,
    readonly invocation?: Invocation,
  ) {
    issued.set(this, this);
  }
  get principal(): string {
    return this.session.kernel.command<string>(KernelOp.RootPrincipal, { root: this.root });
  }
  get signal(): AbortSignal {
    return this.session.signal(this.root);
  }
  check(): void {
    this.session.kernel.command(KernelOp.CheckContext, {
      root: this.root,
      call: this.invocation?.id,
    });
  }
  cancel(): void {
    this.session.kernel.command(KernelOp.CancelRoot, { root: this.root });
    this.session.flush();
  }
  get(
    service: string,
    key: string | readonly string[],
    options: AcquisitionOptions = {},
  ): Promise<DynamicReference> {
    this.check();
    return this.session.get(service, key, options, this);
  }
  resource<T>(name: string): import("../types.ts").Resource<T> {
    this.check();
    return this.session.hostResource<T>(name);
  }
}

export function context(value: unknown, session: Session): Context {
  if (typeof value !== "object" || value === null || issued.get(value)?.session !== session) {
    throw new TembraError(Errors.Forbidden.Context, "An issued context from this host is required");
  }
  const result = issued.get(value)!;
  result.check();
  return result;
}

export function isContext(value: unknown): value is Context {
  return typeof value === "object" && value !== null && issued.has(value);
}
