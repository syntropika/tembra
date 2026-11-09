/** Closed adapter vocabulary. Strings are retained only as the wire representation. */
type ValueOf<T> = T[keyof T];

/** Kernel, native, ownership and lifecycle names are generated from Rust. */
export { ActionType, EffectType, KernelOp, NativeOp, OwnerKind, StepType } from "./commands.ts";

export const RpcOp = Object.freeze({
  Describe: "describe",
  Get: "get",
  Release: "release",
  Call: "call",
  Cancel: "cancel",
} as const);
export type RpcOp = ValueOf<typeof RpcOp>;

export const RpcEvent = Object.freeze({
  State: "state",
} as const);
export type RpcEvent = ValueOf<typeof RpcEvent>;

export const EnvironmentPhase = Object.freeze({
  Declaring: "declaring",
  Prepared: "prepared",
  Live: "live",
  Closed: "closed",
} as const);
export type EnvironmentPhase = ValueOf<typeof EnvironmentPhase>;

export const CoordinationDecision = Object.freeze({
  Preparing: "preparing",
  Commit: "commit",
  Abort: "abort",
} as const);
export type CoordinationDecision = ValueOf<typeof CoordinationDecision>;

export const Outcome = Object.freeze({
  Ok: "ok",
  Error: "error",
} as const);
export type Outcome = ValueOf<typeof Outcome>;

export const CodecTag = Object.freeze({
  Bigint: "bigint",
  Bytes: "bytes",
  Object: "object",
} as const);
export type CodecTag = ValueOf<typeof CodecTag>;

export const CodecId = Object.freeze({
  Json: "tembra-json/1",
  MessagePack: "tembra-msgpack/1",
} as const);
export type CodecId = ValueOf<typeof CodecId>;
