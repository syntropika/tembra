// Generated from crates/executor-wasm/src/abi.rs. Do not edit.
// Regenerate with: TEMBRA_BLESS=1 cargo test -p tembra-executor-wasm --lib abi
export const IMPORT_MODULE =
  "tembra-interpreter-v12-d82248b7b421e5294c2536726adcbbe2c178bc87142fc7ee04faf691081c101b";
export const SYNC_IMPORT = "sync";
export const SYNC_READ_IMPORT = "sync_read";
export const MEMORY_MODULE = "env";
export const MEMORY_IMPORT = "memory";
export const INITIALIZE_EXPORT = "_initialize";
export const ALLOC_EXPORT = "tembra_alloc";
export const CALL_EXPORT = "tembra_call";
export const WASI_MODULE = "wasi_snapshot_preview1";
export const SYNC_ERROR = 2147483648;
export const STANDARD_PACKET = 0;
export const MEMORY_INITIAL_PAGES = 32;
export const PAGE_BYTES = 65536;
export const LENGTH_LIMIT = 2147483647;
export const EFFECT_LIMIT = 128;
export const IDENTITY_LIMIT = 9007199254740991;
export const REPLY_HEADER_BYTES = 20;
export const EFFECT_RECORD_BYTES = 12;
export const COMPLETION_HEADER_BYTES = 4;
export const COMPLETION_RECORD_BYTES = 16;
export const ERRNO_SUCCESS = 0;
export const ERRNO_BADF = 8;
export const ERRNO_FAULT = 21;
export const ERRNO_INVAL = 28;
export const ERRNO_NOTDIR = 54;
export const ERRNO_SPIPE = 70;
export const CLOCK_REALTIME = 0;
export const CLOCK_MONOTONIC = 1;
export const STDIO_DESCRIPTORS = 3;
export const FILETYPE_CHARACTER_DEVICE = 2;
export const RIGHT_FD_WRITE = 64;
export const FDSTAT_BYTES = 24;
export const STDIO_LIMIT = 4096;
export const WASI_IMPORTS = Object.freeze([
  "clock_time_get",
  "environ_get",
  "environ_sizes_get",
  "fd_close",
  "fd_fdstat_get",
  "fd_prestat_dir_name",
  "fd_prestat_get",
  "fd_seek",
  "fd_write",
  "path_open",
  "proc_exit",
] as const);
export const Operation = Object.freeze({
  Load: 0,
  Declare: 1,
  Start: 2,
  Resume: 3,
  Cancel: 4,
  Capture: 5,
} as const);
export type Operation = (typeof Operation)[keyof typeof Operation];
export const ReplyKind = Object.freeze({
  Pending: 0,
  Ok: 1,
  Error: 2,
} as const);
export type ReplyKind = (typeof ReplyKind)[keyof typeof ReplyKind];
export const Status = Object.freeze({
  Ok: 0,
  Error: 1,
} as const);
export type Status = (typeof Status)[keyof typeof Status];
