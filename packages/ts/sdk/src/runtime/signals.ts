import { TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
/** Observe both browser signals and React Native's smaller AbortSignal implementation. */
export function checkSignal(signal: AbortSignal): void {
  if (signal.aborted) throw signal.reason ?? new TembraError(Errors.Cancelled.Root);
}

/** Link cancellation to a callback lifetime and remove fallback listeners when either ends. */
export function combinedSignal(owner: AbortSignal, ended: AbortSignal): AbortSignal {
  if (typeof AbortSignal.any === "function") return AbortSignal.any([owner, ended]);
  const controller = new AbortController();
  const detach = () => {
    owner.removeEventListener("abort", cancelOwner);
    ended.removeEventListener("abort", cancelEnded);
  };
  const cancelOwner = () => {
    detach();
    controller.abort(owner.reason);
  };
  const cancelEnded = () => {
    detach();
    controller.abort(ended.reason);
  };
  if (owner.aborted) cancelOwner();
  else if (ended.aborted) cancelEnded();
  else {
    owner.addEventListener("abort", cancelOwner, { once: true });
    ended.addEventListener("abort", cancelEnded, { once: true });
  }
  return controller.signal;
}
