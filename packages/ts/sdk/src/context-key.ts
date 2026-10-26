/** Stable scalar keys shared across bundles and languages. */
import type { Context } from "./runtime/context.ts";
import { KernelOp } from "./bridge/protocol.ts";

export const ContextKind = Object.freeze({
  Text: "text",
  Integer: "integer",
  Boolean: "boolean",
  Opaque: "opaque",
} as const);
export type ContextKind = (typeof ContextKind)[keyof typeof ContextKind];
/** An owner-issued reference. Its token does not retain the source frame. */
export interface OpaqueContextValue {
  readonly opaque: true;
}
export class ContextKey<T> {
  declare private readonly type: T;

  /** Retain a frozen key contract; Rust validates its use. */
  private constructor(
    readonly name: string,
    readonly version: number,
    readonly kind: ContextKind,
  ) {
    Object.freeze(this);
  }

  /** Declare a text binding with stable identity. */
  static text(name: string, version = 1) {
    return new ContextKey<string>(name, version, ContextKind.Text);
  }

  /** Declare an integer binding with stable identity. */
  static integer(name: string, version = 1) {
    return new ContextKey<number>(name, version, ContextKind.Integer);
  }

  /** Declare a boolean binding with stable identity. */
  static boolean(name: string, version = 1) {
    return new ContextKey<boolean>(name, version, ContextKind.Boolean);
  }

  /** Declare an opaque binding with stable identity. */
  static opaque(name: string, version = 1) {
    return new ContextKey<OpaqueContextValue>(name, version, ContextKind.Opaque);
  }

  /** Resolve a token issued by the owner in this host and root. */
  source(value: T, ctx: Context): number | undefined {
    if (this.kind !== ContextKind.Opaque) return undefined;
    const token =
      typeof value === "object" && value !== null
        ? ctx.session.contextTokens.get(value)
        : undefined;
    if (!token || token.root !== ctx.root)
      throw new TypeError("An owner-issued opaque reference from this host and root is required");
    return token.source;
  }
}

/** Read metadata or wrap an owner-issued local reference. */
export function readContextValue<T>(
  ctx: Context,
  key: ContextKey<T>,
  frame: number,
): T | undefined {
  const entry = ctx.session.kernel.command<{
    kind: ContextKind;
    value?: T;
    source?: number;
  } | null>(KernelOp.ContextValue, {
    root: ctx.root,
    context: frame,
    call: ctx.invocation?.id,
    key,
  });
  if (!entry) return undefined;
  if (entry.kind !== ContextKind.Opaque) return entry.value;
  const token = Object.freeze({ opaque: true as const });
  ctx.session.contextTokens.set(token, { root: ctx.root, source: entry.source! });
  // SAFETY: Rust matched the opaque binding to the supplied typed key.
  return token as T;
}

/** Read an owner-issued reference from a live source frame. */
export function opaqueFrom(
  ctx: Context,
  key: ContextKey<OpaqueContextValue>,
  issuedFrame: number,
): OpaqueContextValue {
  const token = readContextValue(ctx, key, issuedFrame);
  if (!token) throw new TypeError("The owner did not issue a reference");
  return token;
}
