import { Lifetime } from "../bridge/identifiers.ts";

export interface Description {
  name?: string;
  apiVersion?: number;
  services: Record<
    string,
    { methods: string[]; lifetime?: Lifetime; linger?: number; access?: boolean }
  >;
}

import { failure } from "../error.ts";
export type ProviderBinding =
  | { kind: "orb"; name: string; api_version: number }
  | { kind: "host"; provider: string };
export interface Grants {
  filesystem?: readonly { provider: string; mount: string; writable: boolean }[];
  network?: readonly { provider: string; connect: readonly string[] }[];
}
export interface Registration {
  name: string;
  apiVersion: number;
  bindings?: Readonly<Record<string, ProviderBinding>>;
}
export interface ContractPort {
  request(packet: string): string;
  free?(): void;
}
/** Host-issued authority stays local while only data crosses the Wasm boundary. */
export interface ExecutionScope {
  readonly signal?: AbortSignal;
  check(): void;
  cancel(): void;
  sync(request: import("./host-protocol.ts").HostRequest): unknown;
  async(request: import("./host-protocol.ts").AsyncHostRequest): Promise<unknown>;
}
export interface InterpreterPort {
  release(environment: number): void | Promise<void>;
  description(): string;
  declare(packet: string, scope: ExecutionScope): unknown | Promise<unknown>;
  execute(packet: string, scope: ExecutionScope): Promise<unknown>;
  close(): void | Promise<void>;
}

/** Decode a platform result without treating guest error text as host authority. */
export function answer<T>(packet: string): T {
  const value = JSON.parse(packet);
  if (Object.hasOwn(value, "ok")) return value.ok as T;
  throw failure(value.error ?? "Interpreter operation failed");
}
/** Reuse the Rust contract implementation for planning and both boundary validations. */
export function query<T>(contract: ContractPort, value: unknown): T {
  return answer<T>(contract.request(JSON.stringify(value)));
}
export const kebab = (name: string): string => name.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`);
