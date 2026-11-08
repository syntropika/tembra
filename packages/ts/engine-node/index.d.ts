export class NodeKernel {
  /** Execute one metadata request in the Rust engine. */
  request(metadata: string): string;
}

/** Resolve WIT and its bundled standard contracts for build-time type generation. */
export class NodeArtifact {
  constructor(bytes: Uint8Array);
  description(): string;
  request(packet: string): string;
}

export function compileContract(directory: string, world?: string): Uint8Array;

/** Resolved WIT used by optional sandbox platform adapters. */
export class NodeContract {
  constructor(bytes: Uint8Array, world?: string);
  request(packet: string): string;
  artifact(
    source: string | undefined,
    component: Uint8Array | undefined,
    name: string,
    apiVersion: number,
  ): Uint8Array;
  free?(): void;
}

/** Isolated Wasmtime backend used by Tembra's optional interpreter facade. */
export class NodeInterpreter {
  constructor(configuration: string, log: (packet: string) => void);
  interpreter(bytes: Uint8Array): void;
  open(artifact: NodeArtifact, bindings: string, grants: string): NodeInterpreterPort;
  openComponent(artifact: NodeArtifact, configuration: string): NodeComponentPort;
  instances(): number;
}

/** Scoped interpreter transport; application consumers should use the SDK facade. */
export class NodeInterpreterPort {
  release(environment: number): void;
  description(): string;
  declare(packet: string, callback: (packet: string) => string): string;
  dispatch(packet: string, callback: (packet: string) => void): void;
  reply(id: number, packet: string): boolean;
  finish(): void;
  close(): void;
}

/** Data-only compiled-component endpoint used by the shared Orb facade. */
export class NodeComponentPort {
  description(): string;
  call(binding: string, method: string, arguments_: string): Promise<string>;
  close(): void;
}
