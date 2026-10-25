export interface Trace {
  service: string;
  method: string;
  root: number;
  codeVersion: number;
  duration: number;
  outcome: "ok" | "error";
}

/** Bounded opt-in timing. Exporters remain an infrastructure concern. */
export class Telemetry {
  #events: Trace[] = [];
  #next = 0;
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
  sample(): boolean {
    return this.#next++ % this.sampleEvery === 0;
  }
  record(trace: Trace): void {
    if (this.#events.length === this.capacity) this.#events.shift();
    this.#events.push(trace);
  }
  drain(): readonly Trace[] {
    return this.#events.splice(0);
  }
}
