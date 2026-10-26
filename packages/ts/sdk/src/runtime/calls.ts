import type { Completion } from "../authoring/types.ts";
import { Code } from "../bridge/identifiers.ts";
import type { Invocation } from "../bridge/kernel.ts";
import { KernelOp, Outcome } from "../bridge/protocol.ts";
import { TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import { Context, context } from "./context.ts";
import type { Environment, ReferenceRecord } from "./records.ts";
import type { Session } from "./session.ts";

/** Execute JS calls. Rust controls admission and physical lifetime. */
export class Calls {
  /** Connect JS callback execution to the session kernel. */
  constructor(readonly session: Session) {}

  /** Lazily mirror root cancellation into an AbortSignal. */
  signal(root: number): AbortSignal {
    const session = this.session;
    let controller = session.controllers.get(root);
    if (!controller) {
      controller = new AbortController();
      try {
        session.kernel.command(KernelOp.CheckRoot, { root });
      } catch (error) {
        if (!(error instanceof TembraError) || error.code !== Code.Cancelled) throw error;
        controller.abort(error);
      }
      session.controllers.set(root, controller);
    }
    return controller.signal;
  }

  /** Release root-owned references after the callback settles. */
  async root<T>(
    operation: (context: Context) => Completion<T>,
    cleanup = false,
    issuedRoot?: number,
    principal = this.session.principal,
  ): Promise<T> {
    const session = this.session;
    const root =
      issuedRoot ??
      session.kernel.command<number>(cleanup ? KernelOp.CleanupRoot : KernelOp.Root, { principal });
    const ctx = new Context(session, root);
    try {
      return await operation(ctx);
    } finally {
      for (const reference of session.rootReferences.get(root) ?? [])
        session.handles.release(reference);
      session.rootReferences.delete(root);
      session.options.native?.rootEnded?.(root);
      if (!session.lifecycle.closedDiagnostics) session.kernel.command(KernelOp.EndRoot, { root });
      session.controllers.delete(root);
      session.lifecycle.flush();
    }
  }

  /** Execute one admitted service call and retain physical ownership. */
  async invoke(
    reference: ReferenceRecord,
    method: string,
    arguments_: unknown[],
    ctx: Context,
  ): Promise<unknown> {
    const session = this.session;
    context(ctx, session);
    if (reference.released) throw new TembraError(Errors.Released.Reference);
    await reference.ready;
    ctx.check();
    const invocation = session.kernel.command<Invocation>(KernelOp.BeginCall, {
      reference: reference.id,
      root: ctx.root,
      context: ctx.frame || undefined,
      method,
    });
    const entered = new Context(session, ctx.root, invocation, ctx.frame);
    const physical = session.track(
      (async () => {
        const sampled = session.telemetry?.sample();
        const started = sampled ? session.now() : 0;
        let outcome: Outcome = Outcome.Ok;
        try {
          if (session.options.native?.hasOrb(invocation.orb))
            return await session.options.native.invoke(invocation, arguments_, entered);
          const object = session.objects.get(invocation.object);
          const environment = object?.versions.get(invocation.orb);
          const handle =
            environment?.orb.definition.services?.[reference.address.service]?.methods[method];
          if (!environment || !handle) throw new TembraError(Errors.Gone.Implementation);
          return await handle(environment.value, entered, ...arguments_);
        } catch (error) {
          outcome = Outcome.Error;
          throw error;
        } finally {
          if (sampled)
            session.telemetry!.record({
              service: reference.address.service,
              method,
              root: invocation.root,
              codeVersion: invocation.codeVersion,
              duration: session.now() - started,
              outcome,
            });
          session.kernel.command(KernelOp.Tick, { now: session.now() });
          session.kernel.command(KernelOp.FinishCall, { call: invocation.id });
          session.lifecycle.flush();
        }
      })(),
    );
    return this.observe(physical, ctx.signal);
  }

  /** Fresh callback roots inherit owner identity; supplied roots retain the caller's authority. */
  async owned<T>(
    environment: Environment,
    operation: number,
    handle: (ctx: Context) => Completion<T>,
    root?: number | Context,
  ): Promise<T> {
    const session = this.session;
    const execute = async (ctx: Context) => {
      const invocation = session.kernel.command<Invocation>(KernelOp.BeginOperation, {
        orb: environment.orb.info.id,
        root: ctx.root,
        context: ctx.frame || undefined,
        operation,
        ...(environment.object ? { object: environment.object.id } : {}),
      });
      try {
        return await handle(new Context(session, ctx.root, invocation, ctx.frame));
      } finally {
        session.kernel.command(KernelOp.FinishCall, { call: invocation.id });
        session.lifecycle.flush();
      }
    };
    return root === undefined
      ? this.root(execute, false, undefined, session.principalFor(environment.object?.id ?? 0))
      : execute(typeof root === "number" ? new Context(session, root) : root);
  }

  /** Stop waiting on cancellation while physical work remains tracked. */
  async observe<T>(physical: Promise<T>, signal: AbortSignal): Promise<T> {
    if (signal.aborted) throw signal.reason;
    let abort!: () => void;
    const cancelled = new Promise<never>((_, reject) => {
      abort = () => reject(signal.reason ?? new TembraError(Errors.Cancelled.Root));
      signal.addEventListener("abort", abort, { once: true });
    });
    try {
      return await Promise.race([physical, cancelled]);
    } finally {
      signal.removeEventListener("abort", abort);
    }
  }
}
