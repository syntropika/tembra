import type { Lifetime } from "../bridge/identifiers.ts";
import type { KernelOrb } from "../bridge/kernel.ts";
import { failure } from "../error.ts";

/** A host-owned system directory registered as a filesystem provider. */
export interface DirectoryFilesystem {
  readonly kind: "directory";
  readonly path: string;
}

export interface Description {
  name?: string;
  apiVersion?: number;
  services: Record<
    string,
    { methods: string[]; lifetime?: Lifetime; linger?: number; access?: boolean }
  >;
  hooks?: Record<string, "series" | "waterfall" | "bail">;
  contributes?: Record<string, number>;
}

export interface HookBinding {
  name: string;
  alias: string;
  method: string;
  slot: number;
  signature: string;
  composition: "series" | "waterfall" | "bail";
}
export interface ContributionBinding {
  binding: string;
  target: string;
  name: string;
  alias: string;
  method: string;
  slot: number;
  signature: string;
  priority: number;
  optional: boolean;
}
export interface CheckedImplementation {
  services: ServiceBinding[];
  hooks: { hooks: HookBinding[]; contributions: ContributionBinding[] };
  plan: InstallationPlan["plan"] & {
    /** The incoming HTTP export served through `Host.handleHttp`, absent without one. */
    ingress?: string | null;
  };
}

export type ProviderBinding =
  | { kind: "orb"; name: string; api_version: number }
  | { kind: "host"; provider: string };
/** Checked install metadata; the kernel's generated types own shared lifecycle records. */
export interface InstallationPlan {
  bindings: Record<string, ProviderBinding>;
  importServices: Record<string, string>;
  plan: {
    orb: Pick<
      KernelOrb,
      "name" | "apiVersion" | "services" | "requires" | "hooks" | "hookSignatures"
    >;
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
/** The guest service and method that serve incoming HTTP in one environment. */
export interface IngressDispatch {
  environment: number;
  /** The guest's JavaScript service name for the `wasi:http/handler` export. */
  service: string;
  method: string;
}
export interface InterpreterPort {
  release(environment: number): void | Promise<void>;
  description(): string;
  declare(packet: string, scope: ExecutionScope): unknown | Promise<unknown>;
  execute(packet: string, scope: ExecutionScope): Promise<string>;
  /**
   * Serve one incoming HTTP request; resolves only after the response delivery ended and the
   * callback physically settled. Absent where this host cannot serve incoming HTTP yet.
   */
  ingress?(
    dispatch: IngressDispatch,
    exchange: import("./http-ingress.ts").IngressExchange,
    scope: ExecutionScope,
  ): Promise<void>;
  close(): void | Promise<void>;
}

/** Decode a platform result without treating guest error text as host authority. */
export function answer<T>(packet: string): T {
  const value = JSON.parse(packet);
  if (Object.hasOwn(value, "ok")) return value.ok as T;
  throw failure(value.error ?? "Interpreter operation failed");
}
/** Reuse the prepared contract for installation planning. */
export function query<T>(contract: Pick<ContractPort, "request">, value: unknown): T {
  return answer<T>(contract.request(JSON.stringify(value)));
}
