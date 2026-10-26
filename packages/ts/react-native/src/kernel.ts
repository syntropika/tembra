import type { KernelPort } from "@syntropika/tembra-sdk/runtime";
import type { Spec } from "./NativeTembra.ts";

/** Own one native handle for the complete lifetime of a JavaScript host. */
export class NativeKernelPort implements KernelPort {
  #handle: string | undefined;

  /** Allocate a kernel through the platform TurboModule. */
  constructor(private readonly native: Spec) {
    this.#handle = native.createKernel();
  }

  /** Forward metadata without serializing application values or callbacks. */
  request(metadata: string): string {
    if (this.#handle === undefined) throw new Error("The native kernel has been released");
    return this.native.request(this.#handle, metadata);
  }

  /** Release native storage once, after the SDK has drained host cleanup. */
  free(): void {
    if (this.#handle === undefined) return;
    this.native.releaseKernel(this.#handle);
    this.#handle = undefined;
  }
}
