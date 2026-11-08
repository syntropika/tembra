export class NodeKernel {
  /** Execute one metadata request in the Rust engine. */
  request(metadata: string): string;
}

/** Resolve WIT and its bundled standard contracts for build-time type generation. */
export function compileContract(directory: string, world?: string): Uint8Array;

/** Resolved WIT used by optional sandbox platform adapters. */
export class NodeContract {
  constructor(bytes: Uint8Array, world?: string);
  request(packet: string): string;
}

/** Isolated Wasmtime backend used by Tembra's optional interpreter facade. */
export class NodeInterpreter {
  constructor(bytes: Uint8Array, configuration: string, log: (packet: string) => void);
  open(
    name: string,
    apiVersion: number,
    prepared: string,
    bindings: string,
    grants: string,
  ): NodeInterpreterPort;
  openComponent(bytes: Uint8Array, contract: Uint8Array, configuration: string): NodeComponentPort;
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
