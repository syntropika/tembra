import { Outcome } from "./bridge/protocol.ts";
export interface Trace {
  service: string;
  method: string;
  root: number;
  codeVersion: number;
  duration: number;
  outcome: Outcome;
}

/** Bounded opt-in timing. Exporters remain an infrastructure concern. */
export class Telemetry {
  #events: Trace[] = [];
  #next = 0;

  /** Configure a bounded buffer with a fixed sampling interval. */
  constructor(
    readonly capacity = 256,
    readonly sampleEvery = 1,
  ) {
    if (
      !Number.isInteger(capacity) ||
      capacity <= 0 ||
      !Number.isInteger(sampleEvery) ||
      sampleEvery <= 0
    )
      throw new RangeError("Invalid telemetry limits");
  }

  /** Select calls at the configured sampling interval. */
  sample(): boolean {
    return this.#next++ % this.sampleEvery === 0;
  }

  /** Append one sampled call, replacing the oldest record when full. */
  record(trace: Trace): void {
    if (this.#events.length === this.capacity) this.#events.shift();
    this.#events.push(trace);
  }

  /** Return buffered samples and clear their retained references. */
  drain(): readonly Trace[] {
    return this.#events.splice(0);
  }
}
