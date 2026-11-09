import type { Lifetime } from "../bridge/identifiers.ts";
import type { KernelOrb } from "../bridge/kernel.ts";
import { failure } from "../error.ts";

export interface Description {
  name?: string;
  apiVersion?: number;
  services: Record<
    string,
    { methods: string[]; lifetime?: Lifetime; linger?: number; access?: boolean }
  >;
}

export type ProviderBinding =
  | { kind: "orb"; name: string; api_version: number }
  | { kind: "host"; provider: string };
/** Checked install metadata; the kernel's generated types own shared lifecycle records. */
export interface InstallationPlan {
  bindings: Record<string, ProviderBinding>;
  importServices: Record<string, string>;
  plan: {
    orb: Pick<KernelOrb, "name" | "apiVersion" | "services" | "requires" | "hooks">;
  };
}

/** WIT-checked implementation metadata returned by the Rust contract module. */
export interface ServiceBinding {
  name: string;
  interface: string;
  alias: string;
  /** Original WIT method names mapped to guest-facing JavaScript names. */
  methods: Record<string, string>;
  lifetime: Lifetime;
  linger: number;
  signature?: string;
}

export interface ContractPort {
  bind(binding: string, method: string, exports: boolean): number;
  /** Check exact captured JSON text through a bound slot; answers the `{ ok }`/`{ error }` envelope. */
  validate(slot: number, arguments_: boolean, value: string): string;
  request(packet: string): string;
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
  execute(packet: string, scope: ExecutionScope): Promise<string>;
  close(): void | Promise<void>;
}

/** Decode a platform result without treating guest error text as host authority. */
export function answer<T>(packet: string): T {
  const value = JSON.parse(packet);
  if (Object.hasOwn(value, "ok")) return value.ok as T;
  throw failure(value.error ?? "Interpreter operation failed");
}
/** Reuse the prepared contract for installation planning. */
export function query<T>(contract: ContractPort, value: unknown): T {
  return answer<T>(contract.request(JSON.stringify(value)));
}
