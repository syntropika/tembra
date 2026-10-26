import { orb } from "./authoring/orb.ts";
import type {
  AcquisitionOptions,
  Completion,
  OrbDefinition,
  HostOptions,
  ReloadOptions,
  ResourceOptions,
} from "./authoring/types.ts";
import { KernelOp } from "./bridge/protocol.ts";
import { Context } from "./runtime/context.ts";
import { Session } from "./runtime/session.ts";

/** The application facade. Platform entrypoints provide its metadata kernel and module loader. */
export class Host {
  readonly session: Session;

  /** Create the shared session for this host. */
  constructor(options: HostOptions) {
    this.session = new Session(options);
  }

  /** Read the configured host name. */
  get name() {
    return this.session.name;
  }

  /** Read the capabilities compiled into the Rust kernel. */
  get features() {
    return this.session.kernel.features;
  }

  /** Install new orbs and activate their required scopes. */
  install(definition: OrbDefinition | readonly OrbDefinition[]): Promise<void> {
    return this.session.orbs.install(
      Array.isArray(definition) ? definition : [definition as OrbDefinition],
    );
  }

  /** Load an orb with the host's module loader, then install it. */
  async load(location: string | URL): Promise<void> {
    if (!this.session.options.load) throw new Error("This host has no module loader");
    await this.install(await this.session.options.load(location));
  }

  /** Prepare and commit a replacement using the kernel's lifecycle order. */
  reload(definitions: readonly OrbDefinition[], options?: ReloadOptions) {
    return this.session.replacements.replace(definitions, options);
  }

  /** Build a replacement candidate without adopting it. */
  prepare(definitions: readonly OrbDefinition[], options?: ReloadOptions) {
    return this.session.replacements.prepare(definitions, options);
  }

  /** Remove named orbs through the replacement lifecycle. */
  remove(names: readonly string[], options?: ReloadOptions) {
    return this.session.replacements.replace([], options, names);
  }

  /** Acquire a reference before its asynchronous activation completes. */
  hold(service: string, key: string | readonly string[], options?: AcquisitionOptions) {
    return this.session.handles.hold(service, key, options);
  }

  /** Acquire a reference and wait for activation. */
  get(service: string, key: string | readonly string[], options?: AcquisitionOptions) {
    return this.session.handles.get(service, key, options);
  }

  /** Run an operation with one principal and bounded context lifetime. */
  root<T>(
    operation: (context: Context) => Completion<T>,
    options: { principal?: string } = {},
  ): Promise<T> {
    return this.session.calls.root(operation, false, undefined, options.principal);
  }

  /** Declare a resource owned by this host. */
  resource<T>(name: string, options: ResourceOptions<T>) {
    return this.session.scopes.resource(name, options);
  }

  /** Read current kernel counts and pending JS work. */
  diagnostics() {
    return this.session.lifecycle.diagnostics();
  }

  /** Drain errors collected by the host. */
  errors() {
    return this.session.errors.splice(0);
  }

  /** Drain sampled call records. */
  traces() {
    return this.session.telemetry?.drain() ?? [];
  }

  /** Describe installed orbs without activating their services. */
  installedOrbs() {
    return this.session.kernel
      .command<import("./bridge/kernel.ts").KernelOrb[]>(KernelOp.Orbs)
      .map((info) => {
        const definition = this.session.orbs.records.get(info.id)?.definition;
        const description = Object.freeze({
          ...info,
          hookCapabilities: Object.fromEntries(
            info.hooks.map((name) => [name, definition?.hooks?.[name]?.capabilities ?? []]),
          ),
        });
        return Object.freeze({
          ...info,
          /** Return captured metadata without activating services. */
          describe: () => description,
        });
      });
  }

  /** Wait for physical work and cleanup before releasing the kernel. */
  close(): Promise<void> {
    return this.session.lifecycle.close();
  }
}

/** Create a host with an explicitly supplied platform adapter. */
export function createHost(options: HostOptions): Host {
  return new Host(options);
}
export { orb };

export {
  Code,
  HookCapability,
  Lifetime,
  Reason,
  ReloadStatus,
  Status,
} from "./bridge/identifiers.ts";

export { Errors, type ErrorDescriptor } from "./errors.ts";

export type { KernelPort } from "./bridge/kernel.ts";
