import type { TurboModule } from "react-native";
import { TurboModuleRegistry } from "react-native";

export interface Spec extends TurboModule {
  /** Create an isolated native kernel and return its opaque handle. */
  createKernel(): string;
  /** Exchange metadata synchronously with the selected kernel. */
  request(handle: string, metadata: string): string;
  /** Release a kernel and report whether its handle was still live. */
  releaseKernel(handle: string): boolean;
}

export default TurboModuleRegistry.get<Spec>("NativeTembra");
