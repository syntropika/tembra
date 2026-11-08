import type { PreparedSource } from "../tooling/prepare.ts";

export type { PreparedSource };
export type ProviderBinding =
  | { kind: "orb"; name: string; api_version: number; world: string }
  | { kind: "host"; provider: string };
export interface Grants {
  filesystem?: readonly { provider: string; mount: string; writable: boolean }[];
  network?: readonly { provider: string; connect: readonly string[] }[];
}
export interface PreparedComponent {
  readonly format: "tembra-component-v1";
  readonly bytes: readonly number[];
  readonly sha256: string;
  readonly contract: readonly number[];
  readonly world?: string;
}
export interface InterpreterOrbOptions {
  name: string;
  apiVersion: number;
  source: PreparedSource | PreparedComponent;
  bindings?: Readonly<Record<string, ProviderBinding>>;
  grants?: Grants;
}
export interface ContractPort {
  request(packet: string): string;
  free?(): void;
}
export interface InterpreterPort {
  release(environment: number): void | Promise<void>;
  description(): string;
  declare(packet: string, callback: (packet: string) => string): string | Promise<string>;
  dispatch(packet: string, callback: (packet: string) => void): void;
  reply(id: number, packet: string): boolean;
  finish(): void;
  close(): void | Promise<void>;
}

/** Decode a platform result without treating guest error text as host authority. */
export function answer<T>(packet: string): T {
  const value = JSON.parse(packet);
  if (Object.hasOwn(value, "ok")) return value.ok as T;
  throw new Error(
    typeof value.error === "string"
      ? value.error
      : (value.error?.message ?? "Interpreter operation failed"),
  );
}
/** Reuse the Rust contract implementation for planning and both boundary validations. */
export function query<T>(contract: ContractPort, value: unknown): T {
  return answer<T>(contract.request(JSON.stringify(value)));
}
export const camel = (name: string): string =>
  name.replace(/-([a-z])/g, (_, c: string) => c.toUpperCase());
export const kebab = (name: string): string => name.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`);
