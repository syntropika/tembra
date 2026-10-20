/** Stage that failed. Cancelled identifies stopped local work, not rollback of committed effects. */
export type ToolingPhase =
  | "config"
  | "codegen"
  | "typecheck"
  | "bundle"
  | "artifact"
  | "write"
  | "reload"
  | "watch"
  | "cancelled";

/**
 * Inspect phase and reason to distinguish build, publication and delivery failures in Bun tooling.
 * Target identifies a named build when available; cause retains the original rejection or refusal.
 * Messages are diagnostic text. A reload delivery_failed reason can leave the remote outcome unknown;
 * code generation or write failure can retain earlier published files.
 *
 * @example
 * ```ts
 * import { build, loadConfig, ToolingError } from "@tembra/tooling";
 * try {
 *   await build(await loadConfig());
 * } catch (error) {
 *   if (error instanceof ToolingError) {
 *     console.error(error.phase, error.reason, error.target, error.cause);
 *   }
 *   throw error;
 * }
 * ```
 */
export class ToolingError extends Error {
  /** Construct a diagnostic while retaining an existing error through options.cause. */
  constructor(
    /** Tooling stage that detected the failure. */
    readonly phase: ToolingPhase,
    /** Machine-readable reason within phase, such as diagnostics, existing_output or host_rejected. */
    readonly reason: string,
    message: string,
    /** Configuration target name, absent for session-wide and generation errors. */
    readonly target?: string,
    options?: ErrorOptions,
  ) {
    super(message, options);
    this.name = "ToolingError";
  }
}
