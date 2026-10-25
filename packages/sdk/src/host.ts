import { KernelOp } from "./protocol.ts";
import type {
  AcquisitionOptions,
  Completion,
  FacetDefinition,
  HostOptions,
  ReloadOptions,
  ResourceOptions,
} from "./types.ts";
import { Session } from "./runtime/session.ts";
import { Context } from "./runtime/context.ts";
import { facet } from "./facet.ts";

/** The application facade. Platform entrypoints provide its metadata kernel and module loader. */
export class Host {
  readonly session: Session;
  constructor(options: HostOptions) {
    this.session = new Session(options);
  }
  get name() {
    return this.session.name;
  }
  get features() {
    return this.session.kernel.features;
  }
  install(definition: FacetDefinition | readonly FacetDefinition[]): Promise<void> {
    return this.session.facets.install(
      Array.isArray(definition) ? definition : [definition as FacetDefinition],
    );
  }
  async load(location: string | URL): Promise<void> {
    if (!this.session.options.load) throw new Error("This host has no module loader");
    await this.install(await this.session.options.load(location));
  }
  reload(definitions: readonly FacetDefinition[], options?: ReloadOptions) {
    return this.session.replacements.replace(definitions, options);
  }
  prepare(definitions: readonly FacetDefinition[], options?: ReloadOptions) {
    return this.session.replacements.prepare(definitions, options);
  }
  remove(names: readonly string[], options?: ReloadOptions) {
    return this.session.replacements.replace([], options, names);
  }
  hold(service: string, key: string | readonly string[], options?: AcquisitionOptions) {
    return this.session.hold(service, key, options);
  }
  get(service: string, key: string | readonly string[], options?: AcquisitionOptions) {
    return this.session.get(service, key, options);
  }
  root<T>(
    operation: (context: Context) => Completion<T>,
    options: { principal?: string } = {},
  ): Promise<T> {
    return this.session.root(operation, false, undefined, options.principal);
  }
  resource<T>(name: string, options: ResourceOptions<T>) {
    return this.session.resource(name, options);
  }
  diagnostics() {
    return this.session.diagnostics();
  }
  errors() {
    return this.session.errors.splice(0);
  }
  traces() {
    return this.session.telemetry?.drain() ?? [];
  }
  installedFacets() {
    return this.session.kernel
      .command<import("./kernel.ts").KernelFacet[]>(KernelOp.Facets)
      .map((info) => {
        const definition = this.session.facets.records.get(info.id)?.definition;
        const description = Object.freeze({
          ...info,
          hookCapabilities: Object.fromEntries(
            info.hooks.map((name) => [name, definition?.hooks?.[name]?.capabilities ?? []]),
          ),
        });
        return Object.freeze({ ...info, describe: () => description });
      });
  }
  close(): Promise<void> {
    return this.session.close();
  }
}

export function createHost(options: HostOptions): Host {
  return new Host(options);
}
export { facet };

export { Code, Reason, Lifetime, Status, ReloadStatus, HookCapability } from "./identifiers.ts";

export { Errors, type ErrorDescriptor } from "./errors.ts";
