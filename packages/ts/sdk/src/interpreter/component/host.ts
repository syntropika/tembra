/**
 * Host imports of a component: the private `tembra:orb/host` controls of a source Orb, its typed
 * Orb dependency imports and the self-imports through which it runs its own hooks.
 *
 * Every control maps to the existing host request vocabulary served by `Scope`, under the
 * authority of the callback active when it is called. User values never cross the private
 * package: dependency calls and hook runs use the contract's own typed WIT functions, and a
 * `select`/`select-hook` only targets the next such import in the same turn, never authority.
 */
import type { AsyncHostRequest, Handle, HostRequest, LogLevel } from "../host-protocol.ts";
import type { HookBinding } from "../types.ts";
import type { Callback, ComponentPort } from "./port.ts";
import type { FunctionValues } from "./values.ts";

export const HOST = "tembra:orb/host@0.1.0";

/** The JCO form of `tembra:orb/host.handle`. */
interface GuestHandle {
  scope?: bigint;
  id: number;
}
const fail = (error: unknown) => ({
  tag: "err",
  val: error instanceof Error ? error.message : String(error),
});
const ok = (val?: unknown) => (val === undefined ? { tag: "ok" } : { tag: "ok", val });

/** A host-issued environment fits a safe integer; anything else is a forged handle. */
function handle(value: GuestHandle): Handle {
  if (value.scope === undefined) return { id: value.id };
  if (value.scope < 0n || value.scope > BigInt(Number.MAX_SAFE_INTEGER))
    throw new Error("Foreign environment handle");
  return { scope: Number(value.scope), id: value.id };
}

/** Kernel state is JSON text in the private package and a JSON value in the kernel. */
function snapshot(value: unknown) {
  const {
    value: state,
    revision,
    version,
  } = value as {
    value?: unknown;
    revision: unknown;
    version: number;
  };
  return {
    ...(Object.hasOwn(value as object, "value") ? { value: JSON.stringify(state) } : {}),
    revision: String(revision),
    version,
  };
}

/** The private `tembra:orb/host` import object of one instance. */
export function orbHost(
  port: ComponentPort,
  log: (level: LogLevel, message: string) => void,
): Record<string, unknown> {
  const sync = (request: HostRequest) => {
    try {
      return ok(port.current().scope.sync(request));
    } catch (error) {
      return fail(error);
    }
  };
  const map = (request: HostRequest, convert: (value: unknown) => unknown) => {
    try {
      return ok(convert(port.current().scope.sync(request)));
    } catch (error) {
      return fail(error);
    }
  };
  const later = (request: (callback: Callback) => AsyncHostRequest) => {
    const callback = port.current();
    return port.physical(
      callback,
      Promise.resolve()
        .then(() => callback.scope.async(request(callback)))
        .then(
          (value) => ok(value ?? undefined),
          (error) => fail(error),
        ),
    );
  };
  return {
    environment: () => port.current().environment,
    log: (level: LogLevel, message: string) => {
      try {
        log(level, message);
      } catch {
        /* Logging observers do not control the guest. */
      }
    },
    instanceAddress: () => sync({ operation: "address" }),
    root: () =>
      map({ operation: "orb" }, (value) =>
        value === null || value === undefined ? undefined : BigInt(value as number),
      ),
    contextCheck: () => map({ operation: "context.check" }, () => undefined),
    contextPrincipal: () => sync({ operation: "context.principal" }),
    contextCancel: () => map({ operation: "context.cancel" }, () => undefined),
    signalCancelled: () => sync({ operation: "signal.cancelled" }),
    stateDeclare: (initial: string, version: number, validate: number | undefined) => {
      try {
        return sync({
          operation: "state.declare",
          initial: JSON.parse(initial),
          version,
          validate: validate ?? null,
        });
      } catch (error) {
        return fail(error);
      }
    },
    stateRead: (state: GuestHandle, known: string | undefined) => {
      try {
        return map(
          {
            operation: "state.read",
            ...handle(state),
            ...(known === undefined ? {} : { known }),
          },
          snapshot,
        );
      } catch (error) {
        return fail(error);
      }
    },
    stateUpdate: (state: GuestHandle, value: string) => {
      try {
        return map(
          {
            operation: "state.update",
            ...handle(state),
            value: JSON.parse(value),
          },
          snapshot,
        );
      } catch (error) {
        return fail(error);
      }
    },
    hookDeclare: (alias: string) => sync({ operation: "hook.declare", name: alias }),
    referenceDeclare: (binding: string, key: string[]) =>
      sync({ operation: "reference.declare", service: binding, key: [...key] }),
    referenceRelease: (reference: GuestHandle) => {
      try {
        return map({ operation: "reference.release", ...handle(reference) }, () => undefined);
      } catch (error) {
        return fail(error);
      }
    },
    referenceStatus: (reference: GuestHandle) => {
      try {
        return map({ operation: "reference.status", ...handle(reference) }, String);
      } catch (error) {
        return fail(error);
      }
    },
    referenceState: (reference: GuestHandle, known: string | undefined) => {
      try {
        return map(
          {
            operation: "reference.state",
            ...handle(reference),
            ...(known === undefined ? {} : { known }),
          },
          snapshot,
        );
      } catch (error) {
        return fail(error);
      }
    },
    resourceDeclare: (
      name: string,
      version: number,
      open: number,
      close: number,
      dependencies: GuestHandle[],
    ) => {
      try {
        return sync({
          operation: "resource.declare",
          name,
          version,
          open,
          close,
          dependencies: dependencies.map(handle),
        });
      } catch (error) {
        return fail(error);
      }
    },
    resourceRead: (resource: GuestHandle) => {
      try {
        return sync({ operation: "resource.read", ...handle(resource) });
      } catch (error) {
        return fail(error);
      }
    },
    referenceReady: (reference: GuestHandle) =>
      later(() => ({
        operation: "reference.ready",
        ...handle(reference),
      })).then((value) => ((value as { tag: string }).tag === "ok" ? ok() : value)),
    referenceAcquire: (binding: string, key: string[]) =>
      later(() => ({
        operation: "reference.acquire",
        service: binding,
        key: [...key],
      })),
    wait: (milliseconds: number) =>
      later(() => ({ operation: "wait", milliseconds })).then((value) =>
        (value as { tag: string }).tag === "ok" ? ok() : value,
      ),
    select: (reference: GuestHandle | undefined) => {
      try {
        port.select("reference", reference === undefined ? undefined : handle(reference));
        return ok();
      } catch (error) {
        return fail(error);
      }
    },
    selectHook: (hook: GuestHandle | undefined) => {
      try {
        port.select("hook", hook === undefined ? undefined : handle(hook));
        return ok();
      } catch (error) {
        return fail(error);
      }
    },
  };
}

/** One typed import function and its conversions. */
export interface ImportedFunction {
  readonly name: string;
  readonly spelling: string;
  readonly values: FunctionValues;
}

/**
 * A typed Orb dependency import. The call targets the guest's selection from the same turn or,
 * for a compiled component, its default `main` handle. Infrastructure failures (refusal,
 * cancellation, a provider failure) trap: only the function's own WIT `result` is recoverable.
 */
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
        const selected = port.take(callback, "reference");
        const call = callback.scope.call;
        if (!call) throw new Error("This callback cannot call dependencies");
        const input = values.hostArguments(args);
        return port
          .physical(callback, call(selected, binding, spelling, input))
          .then(values.toGuest);
      },
    ]),
  );
}

/** A self-import of one exported hook interface: runs the hook selected in the same turn. */
export function hookImport(
  port: ComponentPort,
  resolve: () => HookBinding | undefined,
  fn: ImportedFunction,
): Record<string, unknown> {
  return {
    [fn.spelling]: (...args: unknown[]) => {
      const hook = resolve();
      if (!hook) throw new Error("The Orb provides no such hook");
      const callback = port.current();
      const selected = port.take(callback, "hook");
      if (!selected) throw new Error(`Hook ${hook.alias} runs only after select-hook`);
      const run = callback.scope.hook;
      if (!run) throw new Error("This callback cannot run hooks");
      const [input] = fn.values.hostArguments(args);
      return port.physical(callback, run(selected, input, hook.alias)).then(fn.values.toGuest);
    },
  };
}
