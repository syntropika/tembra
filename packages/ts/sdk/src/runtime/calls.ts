import { capturedMethod, capturedDataMethod, type CapturedInvocation } from "#tembra-adapters";
import type { Completion } from "../authoring/types.ts";
import { Code } from "../bridge/identifiers.ts";
import type { Invocation } from "../bridge/kernel.ts";
import { KernelOp, Outcome } from "../bridge/protocol.ts";
import { TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import { Context, context } from "./context.ts";
import type { Environment, ReferenceRecord } from "./records.ts";
import type { Session } from "./session.ts";
import { Encoded } from "../interpreter/codec.ts";
import { checkSignal } from "./signals.ts";

/** Execute JS calls. Rust controls admission and physical lifetime. */
export class Calls {
  readonly #rootCapabilities = new Map<number, Set<{ close(): void }>>();
  /** Connect JS callback execution to the session kernel. */
  constructor(readonly session: Session) {}

  /** Invoke callbacks in a microtask and retain their actual promise until physical settlement. */
  execute<T>(operation: () => Completion<T>): Promise<T> {
    return Promise.resolve().then(operation);
  }

  /** Read the root mirror; foreign-issued roots are checked before its first creation. */
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

  /** Revoke claims and cancellation mirrors when either language ends a root. */
  releaseRoot(root: number): void {
    for (const capability of this.#rootCapabilities.get(root) ?? []) capability.close();
    this.#rootCapabilities.delete(root);
    for (const reference of this.session.rootReferences.get(root) ?? [])
      this.session.handles.release(reference);
    this.session.rootReferences.delete(root);
    this.session.controllers.delete(root);
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
      session.kernel.command(cleanup ? KernelOp.CleanupRoot : KernelOp.Root, { principal });
    // A newly issued root is live and uncancelled on this synchronous path. Register its
    // cancellation mirror before user code runs; admission still rechecks after every wait.
    if (issuedRoot === undefined) session.controllers.set(root, new AbortController());
    const ctx = new Context(session, root);
    try {
      return await operation(ctx);
    } finally {
      this.releaseRoot(root);
      session.options.native?.rootEnded?.(root);
      if (!session.lifecycle.closedDiagnostics) session.lifecycle.endRoot(root);
      session.controllers.delete(root);
    }
  }

  /** Let an application stop observing a call while its actual work remains owned. */
  async invoke(
    reference: ReferenceRecord,
    method: string,
    arguments_: unknown[],
    ctx: Context,
    capture?: CapturedInvocation,
  ): Promise<unknown> {
    context(ctx, this.session);
    checkSignal(ctx.signal);
    return this.observe(
      this.invokePhysical(reference, method, arguments_, ctx, capture),
      ctx.signal,
    );
  }

  /** Foreign callbacks retain their dependency until it physically settles, including on cancellation. */
  async invokePhysical(
    reference: ReferenceRecord,
    method: string,
    arguments_: unknown[],
    ctx: Context,
    capture?: CapturedInvocation,
  ): Promise<unknown> {
    const session = this.session;
    context(ctx, session);
    if (reference.released) throw new TembraError(Errors.Released.Reference);
    await this.observe(reference.ready, ctx.signal);
    const admission = session.options.native?.admit?.(reference.id, method, ctx);
    let invocation: Invocation;
    if (admission) invocation = admission.call;
    else
      // One bounded exchange rechecks the caller's context, then admits the call.
      invocation = session.kernel.command(KernelOp.AdmitCall, {
        reference: reference.id,
        root: ctx.root,
        parent: ctx.invocation?.id,
        context: ctx.frame || undefined,
        method,
      });
    const entered = new Context(session, ctx.root, invocation, ctx.frame);
    const physical = session.track(
      (async () => {
        const sampled = session.telemetry?.sample();
        const started = sampled ? session.now() : 0;
        let outcome: Outcome = Outcome.Ok;
        let retainedSource: Promise<void> | undefined;
        let retained: Promise<void> | undefined;
        let physicalFailure: { error: unknown } | undefined;
        let published = false;
        let capabilities: NonNullable<CapturedInvocation["capabilities"]> = [];
        let reported = false;
        const reportFailure = (error: unknown) => {
          outcome = Outcome.Error;
          if (reported) return;
          reported = true;
          const stream = capabilities.find((capability) => capability.kind === "bytes");
          if (stream?.kind === "bytes")
            stream.reportFailure(error, (cause) => session.report(cause));
          else session.report(error);
        };
        const retain = (actual: Promise<void>) => {
          if (retainedSource) {
            if (retainedSource !== actual)
              throw new Error(
                "One prepared invocation cannot retain different physical operations",
              );
            return;
          }
          retainedSource = actual;
          // Observe rejection immediately, while keeping the invocation and any resource loan
          // until both provider completion and processing of its bounded head are finished.
          retained = actual.then(
            () => {},
            (error: unknown) => {
              outcome = Outcome.Error;
              physicalFailure = { error };
            },
          );
        };
        const finish = () => {
          if (sampled)
            session.telemetry!.record({
              service: reference.address.service,
              method,
              root: invocation.root,
              codeVersion: invocation.codeVersion,
              duration: session.now() - started,
              outcome,
            });
          session.lifecycle.finishCall(invocation.id);
        };
        let unlend: (() => void) | undefined;
        try {
          unlend = capture?.receiver?.lend();
          const native = session.options.native;
          if (native && (admission?.native ?? native.hasOrb(invocation.orb))) {
            if (capture?.receiver)
              throw new Error("A data-only handler cannot accept a resource receiver");
            return await native.invoke(invocation, arguments_, entered);
          }
          const object = session.objects.get(invocation.object);
          const environment = object?.versions.get(invocation.orb);
          const handle =
            environment?.orb.definition.services?.[reference.address.service]?.methods[method];
          if (!environment || !handle) throw new TembraError(Errors.Gone.Implementation);
          const prepared = capture && capturedMethod(handle, capture.signature);
          if (prepared) {
            // This channel never enters user arguments. A trusted intermediary cannot forward it.
            const output = await this.execute(() =>
              prepared.invoke(
                environment.value,
                entered,
                capture!.input,
                capture!.receiver,
                retain,
              ),
            );
            if (output instanceof Encoded) capture!.output = output;
            else {
              capabilities = output.capabilities ?? [];
              published = true;
              // The bounded outcome reports failures promptly; it never substitutes for
              // actual task exit once the prepared backend registered physical retention.
              if (output.finished) {
                session.track(
                  output.finished.catch((error: unknown) => {
                    reportFailure(error);
                    throw error;
                  }),
                );
                if (!retainedSource) {
                  for (const capability of capabilities) capability.close();
                  throw new Error(
                    "A prepared continuation must retain its actual physical operation",
                  );
                }
              }
              try {
                checkSignal(entered.signal);
                for (const capability of capabilities) {
                  if (capability.kind === "resource") capability.anchor(session, reference);
                  else {
                    capability.bind(entered);
                    let owned = this.#rootCapabilities.get(entered.root);
                    if (!owned) this.#rootCapabilities.set(entered.root, (owned = new Set()));
                    owned.add(capability);
                  }
                }
                capture!.output = output.value;
                capture!.capabilities = capabilities;
              } catch (error) {
                for (const capability of capabilities) capability.close();
                throw error;
              }
            }
            return undefined;
          }
          if (capture?.receiver)
            throw new Error("A data-only handler cannot accept a resource receiver");
          const data = capturedDataMethod(handle);
          return await this.execute(() =>
            data
              ? data(environment.value, entered, arguments_, retain)
              : handle(environment.value, entered, ...arguments_),
          );
        } catch (error) {
          outcome = Outcome.Error;
          throw error;
        } finally {
          if (retained) {
            session.track(
              retained
                .then(() => {
                  if (published && physicalFailure) reportFailure(physicalFailure.error);
                })
                .finally(() => {
                  unlend?.();
                  finish();
                }),
            );
          } else {
            unlend?.();
            finish();
          }
        }
      })(),
    );
    return physical;
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
      const invocation = session.kernel.command(KernelOp.BeginOperation, {
        orb: environment.orb.info.id,
        root: ctx.root,
        context: ctx.frame || undefined,
        operation,
        ...(environment.object ? { object: environment.object.id } : {}),
      });
      try {
        return await this.execute(() =>
          handle(new Context(session, ctx.root, invocation, ctx.frame)),
        );
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
    if (signal.aborted) throw signal.reason ?? new TembraError(Errors.Cancelled.Root);
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
