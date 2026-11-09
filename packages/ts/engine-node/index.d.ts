export class NodeKernel {
  check(root: number, call?: number): string | null;
  /** Execute one closed kernel operation on structured input; returns `{ ok }` or `{ error }`. */
  command(op: string, input: object): unknown;
}

/** Resolve WIT and its bundled standard contracts for build-time type generation. */
export class NodeArtifact {
  constructor(bytes: Uint8Array);
  description(): string;
  bind(binding: string, method: string, exports: boolean): number;
  validate(slot: number, arguments_: boolean, value: string): string;
  request(packet: string): string;
}

export function compileContract(directory: string, world?: string): Uint8Array;

/** Resolved WIT used by optional sandbox platform adapters. */
export class NodeContract {
  constructor(bytes: Uint8Array, world?: string);
  request(packet: string): string;
  sourceArtifact(source: string, name: string, apiVersion: number): Uint8Array;
  componentArtifact(
    component: Uint8Array,
    name: string,
    apiVersion: number,
    contributes?: Readonly<Record<string, number>>,
  ): Uint8Array;
}

/** Isolated Wasmtime backend used by Tembra's optional interpreter facade. */
export class NodeInterpreter {
  constructor(configuration: string, log: (packet: string) => void);
  interpreter(bytes: Uint8Array): void;
  open(artifact: NodeArtifact, bindings: string, grants: string): NodeInterpreterPort;
  instances(): number;
}

/** Scoped source or compiled Orb transport; application consumers should use the SDK facade. */
export class NodeInterpreterPort {
  io(packet: string): Promise<string>;
  release(environment: number): void;
  description(): string;
  declare(packet: string, callback: (packet: string) => string): string;
  dispatch(packet: string, callback: (packet: string) => void): void;
  reply(id: number, packet: string): boolean;
  finish(): void;
  close(): void;
}
