import { facet } from "./authoring/facet.ts";
import type {
  AcquisitionOptions,
  Completion,
  FacetDefinition,
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

  /** Install new facets and activate their required scopes. */
  install(definition: FacetDefinition | readonly FacetDefinition[]): Promise<void> {
    return this.session.facets.install(
      Array.isArray(definition) ? definition : [definition as FacetDefinition],
    );
  }

  /** Load a facet with the host's module loader, then install it. */
  async load(location: string | URL): Promise<void> {
    if (!this.session.options.load) throw new Error("This host has no module loader");
    await this.install(await this.session.options.load(location));
  }

  /** Prepare and commit a replacement using the kernel's lifecycle order. */
  reload(definitions: readonly FacetDefinition[], options?: ReloadOptions) {
    return this.session.replacements.replace(definitions, options);
  }

  /** Build a replacement candidate without adopting it. */
  prepare(definitions: readonly FacetDefinition[], options?: ReloadOptions) {
    return this.session.replacements.prepare(definitions, options);
  }

  /** Remove named facets through the replacement lifecycle. */
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

  /** Describe installed facets without activating their services. */
  installedFacets() {
    return this.session.kernel
      .command<import("./bridge/kernel.ts").KernelFacet[]>(KernelOp.Facets)
      .map((info) => {
        const definition = this.session.facets.records.get(info.id)?.definition;
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
export { facet };

export {
  Code,
  HookCapability,
  Lifetime,
  Reason,
  ReloadStatus,
  Status,
} from "./bridge/identifiers.ts";

export { Errors, type ErrorDescriptor } from "./errors.ts";
