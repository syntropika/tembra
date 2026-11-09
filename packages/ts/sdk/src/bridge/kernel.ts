import { TembraError } from "../error.ts";
import {
  KernelOp,
  type EmptyInput,
  type Features,
  type KernelInput,
  type KernelOutcome,
  type KernelOutput,
} from "./commands.ts";

/** Kernel metadata shapes, generated from the Rust command table. */
export type {
  Acquisition,
  Action,
  Diagnostics,
  Effect,
  HookObservation,
  Invocation,
  KernelCommands,
  KernelError,
  KernelInput,
  KernelOrb,
  KernelOutcome,
  KernelOutput,
  OrbDeclaration,
  Owner,
  Reload,
  Snapshot,
  Step,
} from "./commands.ts";

/** Implemented by Node-API and browser Wasm bindings, and by string-only platform adapters. */
export interface KernelPort {
  /**
   * Execute one closed kernel operation on structured metadata.
   * Kernel failures are returned as `{ error }`, never thrown.
   */
  command<K extends KernelOp>(op: K, input: KernelInput<K>): KernelOutcome<KernelOutput<K>>;

  /** A fixed scalar scope check; only failures carry encoded error metadata. */
  check(root: number, call?: number): string | null | undefined;

  /** Release the platform binding after host shutdown. */
  free?(): void;
}

export type KernelFeatures = Features;

/** Operations without fields take no input argument. */
export type KernelArguments<K extends KernelOp> =
  KernelInput<K> extends EmptyInput ? [input?: EmptyInput] : [input: KernelInput<K>];

const empty: EmptyInput = Object.freeze({});

export class Kernel {
  readonly features: KernelFeatures;

  /** Read the capabilities of the supplied kernel binding. */
  constructor(readonly port: KernelPort) {
    this.features = this.command(KernelOp.Features);
  }

  /** The hot scope check has no input object or success envelope to marshal. */
  check(root: number, call?: number): void {
    const error = this.port.check(root, call);
    if (error !== undefined && error !== null) throw new TembraError(JSON.parse(error));
  }

  /** Execute one operation; its result type is determined by the operation. */
  command<K extends KernelOp>(op: K, ...[input]: KernelArguments<K>): KernelOutput<K> {
    const outcome = this.port.command(op, input ?? (empty as KernelInput<K>));
    if (outcome.error) throw new TembraError(outcome.error);
    return outcome.ok as KernelOutput<K>;
  }
}
