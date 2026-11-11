import type { HostOptions } from "@syntropika/tembra-sdk";
import {
  createMobileHost,
  type Host,
  type MobileExecutionOptions,
} from "@syntropika/tembra-sdk/runtime";
import NativeTembra from "./NativeTembra.ts";
import { MobileExecution } from "./execution.ts";
import { NativeKernelPort } from "./kernel.ts";

export * from "@syntropika/tembra-sdk";
export type { Host };
export type ReactNativeHostOptions = Omit<HostOptions, "kernel" | "native"> & {
  isolated?: MobileExecutionOptions;
};

/**
 * Refuse runtimes without the language features the shared SDK relies on. Hermes V1 under the
 * bridgeless React Native 0.88.1 runtime provides all three; nothing is polyfilled here.
 */
function requireRuntime(): void {
  const missing = [
    typeof WeakRef === "function" ? undefined : "WeakRef",
    typeof FinalizationRegistry === "function" ? undefined : "FinalizationRegistry",
    typeof Symbol.asyncIterator === "symbol" ? undefined : "Symbol.asyncIterator",
  ].filter((name) => name !== undefined);
  if (missing.length)
    throw new Error(
      `This JavaScript runtime lacks ${missing.join(", ")}; Tembra requires React Native 0.88.1 or newer with Hermes V1.`,
    );
}

/** Run trusted host callbacks and isolated Wasm Orbs through the shared Rust lifecycle engine. */
export function createHost(options: ReactNativeHostOptions = {}): Host {
  requireRuntime();
  if (!NativeTembra) {
    throw new Error("NativeTembra is unavailable. Install the native library and rebuild the app.");
  }
  const kernel = new NativeKernelPort(NativeTembra);
  try {
    return createMobileHost(
      { ...options, kernel },
      new MobileExecution(NativeTembra, kernel, options.isolated ?? {}),
      options.isolated ?? {},
    );
  } catch (error) {
    kernel.free();
    throw error;
  }
}

/** Register the application's private persistent directory without granting it to any Orb. */
export function applicationFilesystem(): { readonly kind: "directory"; readonly path: string } {
  if (!NativeTembra) throw new Error("NativeTembra is unavailable. Rebuild the native app.");
  return Object.freeze({
    kind: "directory",
    path: NativeTembra.filesystemDirectory("application"),
  });
}

/** Register disposable app storage; the operating system may remove its contents. */
export function temporaryFilesystem(): { readonly kind: "directory"; readonly path: string } {
  if (!NativeTembra) throw new Error("NativeTembra is unavailable. Rebuild the native app.");
  return Object.freeze({ kind: "directory", path: NativeTembra.filesystemDirectory("temporary") });
}
