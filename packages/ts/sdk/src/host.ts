import { orb } from "./authoring/orb.ts";
import type {
  AcquisitionOptions,
  Completion,
  OrbInput,
  HostOptions,
  LoadOptions,
  ReloadOptions,
  ResourceOptions,
  DynamicReference,
  ServiceDefinition,
  ServiceReference,
} from "./authoring/types.ts";
import { KernelOp } from "./bridge/protocol.ts";
import { Context } from "./runtime/context.ts";
import { Session } from "./runtime/session.ts";
import {
  checkedRequest,
  HTTP_HANDLER,
  IngressExchange,
  type HandleHttpOptions,
  type HttpHandlerResponse,
  type IncomingHttpRequest,
} from "./interpreter/http-ingress.ts";
import {
  fromFetchRequest,
  isFetchRequest,
  toFetchResponse,
} from "./interpreter/http-ingress-fetch.ts";

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
  install(definition: OrbInput | readonly OrbInput[]): Promise<void> {
    return this.session.track(
      this.session.orbs.install(Array.isArray(definition) ? definition : [definition as OrbInput]),
    );
  }

  /** Load an orb with the host's module loader, then install it. */
  async load(location: string | URL, options?: LoadOptions): Promise<void> {
    if (!this.session.options.load) throw new Error("This host has no module loader");
    await this.install(await this.session.options.load(location, options));
  }

  /** Prepare and commit a replacement using the kernel's lifecycle order. */
  reload(definitions: readonly OrbInput[], options?: ReloadOptions) {
    return this.session.replacements.replace(definitions, options);
  }

  /** Build a replacement candidate without adopting it. */
  prepare(definitions: readonly OrbInput[], options?: ReloadOptions) {
    return this.session.replacements.prepare(definitions, options);
  }

  /** Remove named orbs through the replacement lifecycle. */
  remove(names: readonly string[], options?: ReloadOptions) {
    return this.session.replacements.replace([], options, names);
  }

  /** Acquire a reference before its asynchronous activation completes. */
  hold<const Service extends ServiceDefinition>(
    service: Service,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ): ServiceReference<Service>;
  hold(
    service: string,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ): DynamicReference;
  hold(
    service: string | ServiceDefinition,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ) {
    return this.session.handles.hold(service, key, options);
  }

  /** Acquire a reference and wait for activation. */
  get<const Service extends ServiceDefinition>(
    service: Service,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ): Promise<ServiceReference<Service>>;
  get(
    service: string,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ): Promise<DynamicReference>;
  get(
    service: string | ServiceDefinition,
    key: string | readonly string[],
    options?: AcquisitionOptions,
  ) {
    return this.session.handles.get(service, key, options);
  }

  /** Run an operation with one principal and bounded context lifetime. */
  root<T>(
    operation: (context: Context) => Completion<T>,
    options: { principal?: string } = {},
  ): Promise<T> {
    return this.session.calls.root(operation, false, undefined, options.principal);
  }

  /**
   * Serve one incoming HTTP request with the named Orb's `wasi:http/handler` export. The
   * application owns listening, routing and authentication; this only runs the handler.
   *
   * The promise resolves with the response head as soon as the handler returned it; the body is
   * produced while it is read. The call runs in its own root, which stays open, with its
   * authority, until the delivery ended and all physical work settled: reading the body and
   * trailers to the end, calling `return()` on the body iterator (a client disconnect), or
   * cancelling through `options.signal`. A failure before the head (the handler's
   * `error-code` as `HttpHandlerError`, a trap, a refusal) is reported after that settlement.
   * One request is one callback of the Orb's revision, and the revision timeout covers the
   * whole exchange, body included; a timeout retires the revision.
   *
   * The raw form is lossless. A WHATWG `Request` is answered with a WHATWG `Response`, and a
   * response that form cannot represent is refused.
   */
  handleHttp(
    orb: string,
    request: IncomingHttpRequest,
    options?: HandleHttpOptions,
  ): Promise<HttpHandlerResponse>;
  handleHttp(orb: string, request: Request, options?: HandleHttpOptions): Promise<Response>;
  handleHttp(
    orb: string,
    request: IncomingHttpRequest | Request,
    options: HandleHttpOptions = {},
  ): Promise<HttpHandlerResponse | Response> {
    if (isFetchRequest(request))
      return this.handleHttp(orb, fromFetchRequest(request), options).then(toFetchResponse);
    let exchange: IngressExchange;
    try {
      exchange = new IngressExchange(checkedRequest(request));
    } catch (error) {
      return Promise.reject(error);
    }
    const session = this.session;
    const { signal, principal } = options;
    const served = session.calls.root(
      async (ctx) => {
        const cancel = () => ctx.cancel();
        if (signal?.aborted) cancel();
        else signal?.addEventListener("abort", cancel, { once: true });
        try {
          const reference = await ctx.get(`${orb}::${HTTP_HANDLER}`, ["main"]);
          // The root stays open for the physical call, not for the caller's observation of it.
          await session.calls.invokePhysical(
            session.handles.reference(reference),
            "handle",
            [exchange],
            ctx,
          );
        } finally {
          signal?.removeEventListener("abort", cancel);
        }
      },
      false,
      undefined,
      principal,
    );
    void session.track(served).then(
      () => exchange.settle(),
      (error) => exchange.settle(error),
    );
    return exchange.head;
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
    return this.session.kernel.command(KernelOp.Orbs).map((info) => {
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

export {
  HttpHandlerError,
  type HandleHttpOptions,
  type HttpHandlerResponse,
  type IncomingHttpRequest,
} from "./interpreter/http-ingress.ts";
export type { HttpField } from "./interpreter/http-provider.ts";

export type { KernelPort } from "./bridge/kernel.ts";

// Platform adapters share the isolated lifecycle without importing private implementation files.
export {
  createMobileHost,
  type MobileBackend,
  type MobileArtifact,
  type MobilePort,
  type MobileExecutionOptions,
} from "./interpreter/mobile.ts";
