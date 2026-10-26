import { TembraError } from "../error.ts";
import { Code, Lifetime } from "./identifiers.ts";
import { ActionType, EffectType, KernelOp, OwnerKind } from "./protocol.ts";

/** Implemented by Node-API, browser WASM, and embedded V8's native binding. */
export interface KernelPort {
  /** Exchange one JSON metadata request with the Rust kernel. */
  request(metadata: string): string;

  /** Release the platform binding after host shutdown. */
  free?(): void;
}

export interface KernelFeatures {
  hooks: boolean;
  subscriptions: boolean;
  views: boolean;
  telemetry: boolean;
}
export type Owner =
  | { kind: typeof OwnerKind.Host }
  | { kind: typeof OwnerKind.Orb | typeof OwnerKind.Object; id: number };
export interface KernelOrb {
  id: number;
  owner: number;
  codeVersion: number;
  name: string;
  apiVersion: number;
  services: { name: string; methods: string[]; lifetime: Lifetime; linger: number }[];
  hooks: string[];
  requires: {
    orb: string;
    apiVersion: number;
    services: Record<string, string[]>;
    hooks: string[];
  }[];
}
export interface Acquisition {
  reference: number;
  object: number;
  orb: number;
  codeVersion: number;
  activate: boolean;
}
export interface Invocation {
  id: number;
  root: number;
  object: number;
  orb: number;
  codeVersion: number;
  method: string;
}
export interface Snapshot {
  value: number;
  revision: string;
  version: number;
}
export interface Reload {
  id: number;
  codeVersion: number;
  orbs: KernelOrb[];
  objects: number[];
  providers: Record<string, number>;
}
export interface Effect {
  type: EffectType;
  scope?: number;
  previous?: number | null;
  resource?: number;
  value?: number | null;
  version?: number;
  keepListeners?: boolean;
  replacements?: { previous: number | null; next: number | null }[];
}
export interface Action {
  type: ActionType;
  id: number;
  revision?: string;
  effects?: Effect[];
  payload?: number;
  operation?: number;
  error?: { code: Code; reason: string; message: string } | null;
}
export interface Diagnostics {
  objects: number;
  references: number;
  roots: number;
  calls: number;
  resources: number;
  tasks: number;
  orbs: number;
  closing: boolean;
  codeVersion: number;
}

export class Kernel {
  readonly features: KernelFeatures;

  /** Read the capabilities of the supplied kernel binding. */
  constructor(readonly port: KernelPort) {
    this.features = this.command(KernelOp.Features);
  }

  /** Decode a kernel result or raise its structured error. */
  command<T = null>(op: KernelOp, fields: Record<string, unknown> = {}): T {
    const envelope = JSON.parse(this.port.request(JSON.stringify({ ...fields, op }))) as {
      ok?: T;
      error?: { code: Code; reason: string; message: string };
    };
    if (envelope.error) throw new TembraError(envelope.error);
    return envelope.ok as T;
  }
}
