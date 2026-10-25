import { Errors } from "./errors.ts";
import { CoordinationDecision } from "./protocol.ts";

import { TembraError } from "./error.ts";

export interface Participant<T> {
  prepare(signal: AbortSignal): Promise<T>;
  commit(ticket: T): Promise<void>;
  abort(ticket: T): Promise<void>;
}

/** Bounded direct participants. A commit decision is not a durable distributed transaction. */
export async function coordinate<T>(
  participants: readonly Participant<T>[],
  options: { deadline?: number; maxParticipants?: number } = {},
): Promise<void> {
  const deadline = options.deadline ?? 5000;
  if (
    participants.length > (options.maxParticipants ?? 32) ||
    !Number.isFinite(deadline) ||
    deadline <= 0
  )
    throw new TembraError(Errors.Invalid.CoordinationLimits);
  const controller = new AbortController();
  const prepared = new Map<Participant<T>, T>();
  let decision: CoordinationDecision = CoordinationDecision.Preparing;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const preparation = Promise.all(
    participants.map(async (participant) => {
      const ticket = await participant.prepare(controller.signal);
      if (decision === CoordinationDecision.Abort) await participant.abort(ticket);
      else prepared.set(participant, ticket);
    }),
  );
  try {
    await Promise.race([
      preparation,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new TembraError(Errors.Conflict.PrepareDeadline)),
          deadline,
        );
      }),
    ]);
    decision = CoordinationDecision.Commit;
  } catch (error) {
    decision = CoordinationDecision.Abort;
    controller.abort(error);
    const aborted = await Promise.allSettled(
      [...prepared].map(([participant, ticket]) => participant.abort(ticket)),
    );
    preparation.catch(() => {});
    const errors = aborted
      .filter((result) => result.status === "rejected")
      .map((result) => result.reason);
    if (errors.length)
      throw new AggregateError([error, ...errors], "Preparation and cleanup failed");
    throw error;
  } finally {
    clearTimeout(timer);
  }
  const committed = await Promise.allSettled(
    [...prepared].map(([participant, ticket]) => participant.commit(ticket)),
  );
  const errors = committed
    .filter((result) => result.status === "rejected")
    .map((result) => result.reason);
  if (errors.length)
    throw new AggregateError(
      errors,
      "Commit was decided but some participants did not acknowledge it",
    );
}
