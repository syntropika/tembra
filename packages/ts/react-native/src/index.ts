import type { HostOptions } from "@syntropika/tembra-sdk";
import { Host } from "@syntropika/tembra-sdk/runtime";
import NativeTembra from "./NativeTembra.ts";
import { NativeKernelPort } from "./kernel.ts";

export * from "@syntropika/tembra-sdk";
export type { Host };
export type ReactNativeHostOptions = Omit<HostOptions, "kernel" | "native">;

/** Run bundled orbs in the current JS realm using the native Rust lifecycle engine. */
export function createHost(options: ReactNativeHostOptions = {}): Host {
  if (!NativeTembra) {
    throw new Error("NativeTembra is unavailable. Install the native library and rebuild the app.");
  }
  const kernel = new NativeKernelPort(NativeTembra);
  try {
    return new Host({ ...options, kernel });
  } catch (error) {
    kernel.free();
    throw error;
  }
}
