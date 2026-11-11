/**
 * Typed Orb dependency imports of a compiled component.
 *
 * Each call acts for the callback that owns the guest's current root export task (read
 * synchronously at import entry), targets the root environment's default `main` reference, and
 * stays physically accounted to that callback until it settles. Infrastructure failures (refusal,
 * cancellation, a provider failure) reject, which the generated suspending import turns into a
 * trap: only the function's own WIT `result` is recoverable.
 */
import type { ComponentPort } from "./port.ts";
import type { FunctionValues } from "./values.ts";

/** One typed import function and its conversions. */
export interface ImportedFunction {
  /** The WIT function name. */
  readonly name: string;
  /** The JavaScript spelling the generated bindings call. */
  readonly spelling: string;
  readonly values: FunctionValues;
}

/** The import object of one Orb dependency binding. */
export function dependencyImport(
  port: ComponentPort,
  binding: string,
  functions: readonly ImportedFunction[],
): Record<string, unknown> {
  return Object.fromEntries(
    functions.map(({ spelling, values }) => [
      spelling,
      (...args: unknown[]) => {
        const callback = port.current();
        const input = values.hostArguments(args);
        return port
          .physical(callback, callback.scope.call(binding, spelling, input))
          .then(values.toGuest);
      },
    ]),
  );
}
