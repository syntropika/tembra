import { HookCapability } from "../bridge/identifiers.ts";
import { TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import type { HookDefinition } from "./types.ts";

/** Run participants in order with the same input. */
export function series(): HookDefinition {
  return {
    capabilities: [HookCapability.Observe],
    /** Await each participant before starting the next. */
    async run(ctx, input, participants) {
      for (const participant of participants) await participant(ctx, input);
    },
  };
}

/** Pass each result to the next participant. */
export function waterfall(): HookDefinition {
  return {
    capabilities: [HookCapability.Transform],
    /** Feed each result into the next participant. */
    async run(ctx, value, participants) {
      for (const participant of participants) value = await participant(ctx, value);
      return value;
    },
  };
}

/** Return the first result that is not undefined. */
export function bail(): HookDefinition {
  return {
    capabilities: [HookCapability.Decide],
    /** Stop at the first defined result. */
    async run(ctx, value, participants) {
      for (const participant of participants) {
        const result = await participant(ctx, value);
        if (result !== undefined) return result;
      }
    },
  };
}

/** Wait for all participants before returning or failing. */
export function parallel(): HookDefinition {
  return {
    capabilities: [HookCapability.Observe],
    /** Wait for every participant, including those still running after failure. */
    async run(ctx, value, participants) {
      const settled = await Promise.allSettled(
        participants.map((participant) => participant(ctx, value)),
      );
      const error = settled.find((result) => result.status === "rejected");
      if (error?.status === "rejected") throw error.reason;
      return settled.map((result) => (result.status === "fulfilled" ? result.value : undefined));
    },
  };
}

/** Compose participants with single-use continuations. */
export function middleware(
  terminal: (context: import("../runtime/context.ts").Context, input: any) => unknown,
): HookDefinition {
  return {
    capabilities: [HookCapability.Transform, HookCapability.Decide],
    /** Keep continuations valid only while this hook run remains active. */
    async run(ctx, input, participants) {
      let active = true;
      /** Keep each continuation inside its participant's physical lifetime. */
      async function invoke(index: number, value: unknown): Promise<unknown> {
        if (!active) throw new TembraError(Errors.Expired.Next, "Continuation scope has ended");
        ctx.check();
        const participant = participants[index];
        if (!participant) return terminal(ctx, value);
        let used = false;
        let pending: Promise<unknown> | undefined;
        let open = true;
        const next = (nextValue: unknown) => {
          if (!open || !active) return Promise.reject(new TembraError(Errors.Expired.Next));
          if (used) return Promise.reject(new TembraError(Errors.Invalid.NextUsed));
          used = true;
          pending = invoke(index + 1, nextValue);
          // A caller may catch the rejection normally; no special recovery operation exists.
          pending.catch(() => {});
          return pending;
        };
        try {
          return await participant(ctx, value, next);
        } finally {
          open = false;
          // A detached continuation still owns physical work until it settles.
          if (pending) await pending.catch(() => {});
        }
      }
      try {
        return await invoke(0, input);
      } finally {
        active = false;
      }
    },
  };
}
