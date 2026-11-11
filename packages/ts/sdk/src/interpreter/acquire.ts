/**
 * `Context.get` acquisition shared by source scopes and compiled component scopes.
 *
 * The guest names only an import binding (already resolved to its qualified service) and a key.
 * The host resolves the installed requirement's API version and methods, acquires under the
 * admitted invocation's root (and therefore its principal), and awaits readiness under the
 * callback's cancellation. A claim whose wait, authority or publication fails is released before
 * any guest handle exists; the runtime's root claim list releases a published claim when its root
 * ends.
 */
import type { Declaration } from "../authoring/declaration.ts";
import type { DynamicReference } from "../authoring/types.ts";
import type { Context } from "../runtime/context.ts";
import { checkSignal, combinedSignal } from "../runtime/signals.ts";

/**
 * Acquire `service` at `key` for the admitted invocation of `context` and publish it with
 * `publish` once ready. `ended` is the issuing callback's own lifetime.
 */
export async function acquireReference<T>(
  declaration: Declaration,
  service: string,
  key: readonly string[],
  context: Context,
  ended: AbortSignal,
  publish: (reference: DynamicReference) => T,
): Promise<T> {
  context.check();
  // Only an admitted method call selects instances. A cleanup context (a resource `close`) has
  // no invocation; acquiring there would choose instances by the cleanup principal.
  if (!context.invocation) throw new Error("Reference acquisition requires an admitted invocation");
  const session = declaration.session;
  const requirement = session.scopes.requirement(declaration.environment, service);
  const reference = session.handles.hold(
    requirement.service,
    key,
    {
      ...(requirement.apiVersion ? { apiVersion: requirement.apiVersion } : {}),
      ...(requirement.methods ? { methods: requirement.methods } : {}),
    },
    context,
  );
  try {
    await session.calls.observe(reference.ready, combinedSignal(context.signal, ended));
    // The invocation, root or callback may have ended while readiness was pending.
    context.check();
    checkSignal(ended);
    return publish(reference);
  } catch (error) {
    // A secondary release failure is reported to the session instead of replacing the cause.
    try {
      reference.release();
    } catch (release) {
      session.report(release);
    }
    throw error;
  }
}
