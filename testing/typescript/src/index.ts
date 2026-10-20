/** Portable test support. These helpers do not create or emulate a Tembra runtime. */
export {
  check,
  deferredVoid,
  defined,
  reason,
  structuredError,
  assertQuiescent,
} from "./assertions.ts";
export {
  portableHooks,
  javascriptOpaque,
  hookBoundaries,
  hookPhysicalSettlement,
} from "./conformance.ts";
