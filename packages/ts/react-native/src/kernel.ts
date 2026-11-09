import type { KernelPort } from "@syntropika/tembra-sdk/runtime";
import type { Spec } from "./NativeTembra.ts";

/** Own one native handle for the complete lifetime of a JavaScript host. */
export class NativeKernelPort implements KernelPort {
  #handle: string | undefined;

  /** Allocate a kernel through the platform TurboModule. */
  constructor(private readonly native: Spec) {
    this.#handle = native.createKernel();
  }

  /** Read the issued native host identity for its execution adapter. */
  get handle(): string {
    if (this.#handle === undefined) throw new Error("The native kernel has been released");
    return this.#handle;
  }

  /**
   * Exchange one typed command as the JSON text the TurboModule accepts. The native kernel decodes
   * it into the same strictly checked command; application values and callbacks never cross.
   */
  command: KernelPort["command"] = (op, input) => {
    if (this.#handle === undefined) throw new Error("The native kernel has been released");
    return JSON.parse(this.native.request(this.#handle, JSON.stringify({ ...input, op })));
  };

  /** String-only platforms adapt the scalar scope check to the same typed command. */
  check(root: number, call?: number): string | undefined {
    const outcome = this.command("check-context", { root, call });
    return outcome.error === undefined ? undefined : JSON.stringify(outcome.error);
  }

  /** Release native storage once, after the SDK has drained host cleanup. */
  free(): void {
    if (this.#handle === undefined) return;
    this.native.releaseKernel(this.#handle);
    this.#handle = undefined;
  }
}
