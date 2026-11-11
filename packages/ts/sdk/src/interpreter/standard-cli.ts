/** Host-owned byte providers for standard CLI streams, with no inherited process environment. */
import type { CliOptions } from "../authoring/types.ts";

/** The promise owns the actual read until it returns, including after signal cancellation. */
export interface StdioInput {
  read(maxBytes: number, signal: AbortSignal): Promise<Uint8Array | undefined>;
  close?(): void | Promise<void>;
}

/** The promise settles only after the physical write has returned. Calls are serialized. */
export interface StdioOutput {
  write(bytes: Uint8Array, signal: AbortSignal): Promise<void>;
  close?(): void | Promise<void>;
}

/** A fresh set per revision; an output provider can be shared by stdout and stderr. */
export interface StdioStreams {
  readonly stdin?: StdioInput;
  readonly stdout?: StdioOutput;
  readonly stderr?: StdioOutput;
}

/** The embedding chooses adapters; Tembra owns their operation promises and final close. */
export type StdioProvider = (orb: string, configuration: Readonly<CliOptions>) => StdioStreams;
