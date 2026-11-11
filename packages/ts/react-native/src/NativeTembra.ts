import type { TurboModule } from "react-native";
import { TurboModuleRegistry } from "react-native";

export interface Spec extends TurboModule {
  /** Create an isolated native kernel and return its opaque handle. */
  createKernel(): string;
  filesystemDirectory(kind: string): string;
  /** Exchange metadata synchronously with the selected kernel. */
  request(handle: string, metadata: string): string;
  /** Release a kernel and report whether its handle was still live. */
  releaseKernel(handle: string): boolean;
  /** Configure portable execution and host providers; the native library supplies the shared interpreter. */
  configureExecution(handle: string, configuration: string): Promise<void>;
  createArtifact(handle: string, bytes: string): string;
  artifactDescription(handle: string, artifact: string): string;
  artifactRequest(handle: string, artifact: string, packet: string): string;
  artifactBind(
    handle: string,
    artifact: string,
    binding: string,
    method: string,
    exports: boolean,
  ): number;
  artifactValidate(
    handle: string,
    artifact: string,
    slot: number,
    arguments_: boolean,
    value: string,
  ): string;
  releaseArtifact(handle: string, artifact: string): boolean;
  openPort(
    handle: string,
    artifact: string,
    bindings: string,
    grants: string,
    cli: string,
  ): Promise<string>;
  portDescription(handle: string, port: string): string;
  portLogs(handle: string, port: string): string;
  portBegin(handle: string, port: string, packet: string, declaration: boolean): boolean;
  portReceive(handle: string, port: string): Promise<string>;
  portReply(handle: string, port: string, id: number, packet: string): boolean;
  portFinish(handle: string, port: string): boolean;
  abortPort(handle: string, port: string): Promise<void>;
  portRelease(handle: string, port: string, environment: number): boolean;
  portCancel(handle: string, port: string): boolean;
  closePort(handle: string, port: string): boolean;
  /**
   * Incoming HTTP over the port's typed carrier. Metadata JSON carries only control tags, the
   * method, target, status, field names and byte lengths; every field value and body byte travels
   * in the `ArrayBuffer`, field values concatenated in their order (duplicates kept).
   */
  portBeginIngress(
    handle: string,
    port: string,
    packet: string,
    head: string,
    fields: ArrayBuffer,
  ): boolean;
  /**
   * Resolve the next event's metadata. A `head`, `data`, `trailers` or `refused` event leaves one
   * buffer with the port, which `portTakeIngress` must take before the next receive.
   */
  portReceiveIngress(handle: string, port: string): Promise<string>;
  /** Take the received event's buffer; it never waits and refuses when none is pending. */
  portTakeIngress(handle: string, port: string): ArrayBuffer;
  portSupplyRequest(handle: string, port: string, chunk: string, bytes: ArrayBuffer): boolean;
  portPullResponse(handle: string, port: string): boolean;
  portDisconnect(handle: string, port: string): boolean;
}

export default TurboModuleRegistry.get<Spec>("NativeTembra");
