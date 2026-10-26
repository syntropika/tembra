import type { Action, Diagnostics } from "../bridge/kernel.ts";
import { ActionType, KernelOp, OwnerKind } from "../bridge/protocol.ts";
import { TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import type { Session } from "./session.ts";

/** Dispatch core actions and wait for physical shutdown. */
export class Lifecycle {
  /** Connect kernel actions to JS cleanup and scheduling. */
  constructor(readonly session: Session) {}

  /** Read final counts after kernel storage has been released. */
  get closedDiagnostics(): ReturnType<Lifecycle["diagnostics"]> | undefined {
    return this.#finalDiagnostics;
  }

  #closed?: Promise<void>;
  #flushing = false;
  #linger: ReturnType<typeof setTimeout> | undefined;
  #lingerDeadline: number | undefined;
  #finalDiagnostics: ReturnType<Lifecycle["diagnostics"]> | undefined;

  /** Drain kernel actions without re-entering the dispatcher. */
  flush(initial?: Action[]): void {
    if (this.#flushing) return;
    this.#flushing = true;
    try {
      for (;;) {
        const actions = initial ?? this.session.kernel.command<Action[]>(KernelOp.Drain);
        initial = undefined;
        if (!actions.length) break;
        for (const action of actions) {
          try {
            this.action(action);
          } catch (error) {
            this.session.report(error);
          }
        }
      }
    } finally {
      this.#flushing = false;
      this.scheduleLinger();
    }
  }

  /** Compose kernel finish bookkeeping through the embedded adapter when available. */
  finishCall(call: number): void {
    const session = this.session;
    if (!this.#flushing && session.options.native?.finish) {
      const batch = session.options.native.finish(call, session.now());
      if (!batch.actions.length) {
        this.scheduleLinger(batch.deadline);
        return;
      }
      this.flush(batch.actions);
      return;
    }
    session.kernel.command(KernelOp.Tick, { now: session.now() });
    session.kernel.command(KernelOp.FinishCall, { call });
    this.flush();
  }

  /** Execute one action using its owning JS module. */
  private action(action: Action): void {
    const session = this.session;
    const nativeHandled = session.options.native?.notify?.(action);
    if (
      nativeHandled &&
      !(action.type === ActionType.ProgramComplete && session.programs.pending(action.id))
    )
      return;
    switch (action.type) {
      case ActionType.Execute:
      case ActionType.CancelEffects:
      case ActionType.ProgramPrepared:
      case ActionType.ProgramComplete:
        session.programs.dispatch(action);
        break;
      case ActionType.DropValue:
        session.values.drop(action.id);
        session.options.native?.dropValue?.(action.id);
        break;
      case ActionType.CancelRoot:
        session.controllers.get(action.id)?.abort(new TembraError(Errors.Cancelled.Root));
        break;
      case ActionType.CancelTask:
        session.activities.cancel(action.id);
        break;
      case ActionType.Deliver:
        session.activities.deliver(action.id, action.payload!, action.operation!);
        break;
      case ActionType.DetachSubscription:
        session.activities.detach(action.id);
        break;
      case ActionType.SubscriptionFailed:
        session.activities.failed(action.id);
        break;
      case ActionType.Dispose:
        session.track(this.dispose(action.id), true);
        break;
      case ActionType.CloseResource: {
        // Resource callbacks execute only in response to the engine's ordered program effect.
        break;
      }
      case ActionType.RetireOrb:
        session.handles.retire(action.id);
        session.orbs.retire(action.id);
        break;
      case ActionType.State: {
        const object = session.objects.get(action.id);
        if (!object?.binding) break;
        const snapshot = session.snapshot(action.id);
        // Local updates already staged their selectors. Foreign Rust updates enter JS only here.
        if (!session.values.has(snapshot.value)) {
          const candidate = session.value<any>(snapshot.value);
          session.publish(object.binding.adopt(object.binding.prepare(candidate), candidate));
        }
        break;
      }
    }
  }

  /** Wait for object activation and cleanup before releasing its binding. */
  private async dispose(id: number): Promise<void> {
    const session = this.session;
    const object = session.objects.get(id);
    if (!object) {
      // Native contexts can acquire objects before any JS reference mirrors them.
      await session.options.native?.dispose(id);
      session.kernel.command(KernelOp.Disposed, { object: id });
      this.flush();
      return;
    }
    object.disposing ??= (async () => {
      const errors: unknown[] = [];
      await object.activation.catch(() => {});
      const scopes = session.kernel.command<number[]>(KernelOp.ScopesForOwner, {
        owner: { kind: OwnerKind.Object, id },
      });
      try {
        await session.programs.cleanupScopes(scopes);
      } catch (error) {
        errors.push(error);
      }
      try {
        if (session.options.native?.hasOrb(object.orb)) await session.options.native.dispose(id);
      } catch (error) {
        errors.push(error);
      }
      object.binding?.close();
      session.kernel.command(KernelOp.Disposed, { object: id });
      session.objects.delete(id);
      this.flush();
      if (errors.length) throw new AggregateError(errors, "Object cleanup failed");
    })();
    return object.disposing;
  }

  /** Keep one timer for the kernel's earliest deadline. */
  private scheduleLinger(known?: number | null): void {
    const session = this.session;
    if (session.closing) return;
    const deadline =
      (known === undefined
        ? session.kernel.command<number | null>(KernelOp.NextDeadline)
        : known) ?? undefined;
    if (deadline === this.#lingerDeadline) return;
    if (this.#linger !== undefined) clearTimeout(this.#linger);
    this.#linger = undefined;
    this.#lingerDeadline = deadline;
    if (deadline === undefined) return;
    this.#linger = setTimeout(
      () => {
        if (this.#lingerDeadline !== deadline) return;
        this.#linger = undefined;
        this.#lingerDeadline = undefined;
        session.kernel.command(KernelOp.Tick, { now: session.now() });
        this.flush();
      },
      Math.max(0, deadline - session.now()),
    );
    this.#linger.unref?.();
  }

  /** Combine kernel counts with retained JS values and jobs. */
  diagnostics(): Diagnostics & { values: number; pendingJobs: number } {
    const session = this.session;
    if (this.#finalDiagnostics) return this.#finalDiagnostics;
    return {
      ...session.kernel.command<Diagnostics>(KernelOp.Diagnostics),
      values: session.values.size,
      pendingJobs: session.jobs.size,
    };
  }

  /** Wait for physical cleanup before releasing the platform binding. */
  close(): Promise<void> {
    const session = this.session;
    this.#closed ??= (async () => {
      session.closing = true;
      clearTimeout(this.#linger);
      this.#linger = undefined;
      this.#lingerDeadline = undefined;
      session.kernel.command(KernelOp.Close);
      await session.replacements.cancel();
      this.flush();
      for (;;) {
        await session.options.native?.settled?.();
        this.flush();
        const pending = [...session.jobs];
        if (!pending.length) break;
        await Promise.allSettled(pending);
        this.flush();
      }
      session.kernel.command(KernelOp.FinishClose);
      session.values.clear();
      session.handles.clear();
      session.orbs.records.clear();
      session.scopes.clear();
      this.#finalDiagnostics = this.diagnostics();
      session.kernel.port.free?.();
      if (session.cleanupErrors.length)
        throw new AggregateError(session.cleanupErrors, "Host cleanup failed");
    })();
    return this.#closed;
  }
}
