import { TembraError } from "./error.ts";

/** Implemented by Node-API, browser WASM, and embedded V8's native binding. */
export interface KernelPort {
  request(metadata: string): string;
  free?(): void;
}

export interface KernelFeatures {
  hooks: boolean;
  subscriptions: boolean;
  views: boolean;
  telemetry: boolean;
}
export type Owner = { kind: "host" } | { kind: "facet" | "object"; id: number };
export interface KernelFacet {
  id: number;
  owner: number;
  codeVersion: number;
  name: string;
  apiVersion: number;
  services: { name: string; methods: string[]; lifetime: "referenced" | "host"; linger: number }[];
  hooks: string[];
  requires: {
    facet: string;
    apiVersion: number;
    services: Record<string, string[]>;
    hooks: string[];
  }[];
}
export interface Acquisition {
  reference: number;
  object: number;
  facet: number;
  codeVersion: number;
  activate: boolean;
}
export interface Invocation {
  id: number;
  root: number;
  object: number;
  facet: number;
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
  facets: KernelFacet[];
  objects: number[];
}
export interface Effect {
  type: string;
  scope?: number;
  previous?: number | null;
  resource?: number;
  value?: number | null;
  version?: number;
  keepListeners?: boolean;
  replacements?: { previous: number | null; next: number | null }[];
}
export interface Action {
  type: string;
  id: number;
  revision?: string;
  effects?: Effect[];
  payload?: number;
  operation?: number;
  error?: { code: string; reason: string; message: string } | null;
}
export interface Diagnostics {
  objects: number;
  references: number;
  roots: number;
  calls: number;
  resources: number;
  tasks: number;
  facets: number;
  closing: boolean;
  codeVersion: number;
}

export class Kernel {
  readonly features: KernelFeatures;
  constructor(readonly port: KernelPort) {
    this.features = this.command("features");
  }

  command<T = null>(op: string, fields: Record<string, unknown> = {}): T {
    const envelope = JSON.parse(this.port.request(JSON.stringify({ ...fields, op }))) as {
      ok?: T;
      error?: { code: string; reason: string; message: string };
    };
    if (envelope.error)
      throw new TembraError(envelope.error.code, envelope.error.reason, envelope.error.message);
    return envelope.ok as T;
  }
}
