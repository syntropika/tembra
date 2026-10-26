import { Declaration } from "../authoring/declaration.ts";
import type { Resource, ResourceOptions } from "../authoring/types.ts";
import { Code, Reason, Status } from "../bridge/identifiers.ts";
import type { Acquisition, KernelFacet, Owner } from "../bridge/kernel.ts";
import { EnvironmentPhase, KernelOp, OwnerKind } from "../bridge/protocol.ts";
import { refuse, TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import { StateBinding } from "../state.ts";
import type { Environment, FacetRecord, ObjectRecord, ResourceRecord } from "./records.ts";
import type { Session } from "./session.ts";

/** Declare JS scopes and execute activation effects. */
export class Scopes {
  /** Track host resource names and pending host activations. */
  constructor(readonly session: Session) {}

  #hostResources = new Map<string, ResourceRecord>();
  #pendingHostScopes = new Set<Environment>();
  #hostActivation: Promise<void> | undefined;

  /** Allocate the JS callback storage for one declared scope. */
  environment(owner: Owner, facet: FacetRecord, object?: ObjectRecord): Environment {
    const environment: Environment = {
      id: this.session.kernel.command<number>(KernelOp.Allocate),
      owner,
      facet,
      ...(object ? { object } : {}),
      phase: EnvironmentPhase.Declaring,
      value: {},
      dependencies: new Map(),
      resources: new Map(),
      tasks: [],
      listeners: [],
    };
    this.session.programs.scopes.set(environment.id, environment);
    return environment;
  }

  /** Declare native or JS object scopes and wait for readiness. */
  async activate(object: ObjectRecord, acquired: Acquisition): Promise<void> {
    const session = this.session;
    if (session.options.native?.hasFacet(acquired.facet)) {
      await session.options.native.activate(
        object.id,
        acquired.facet,
        object.address.service,
        object.address.key,
        object.address.principal,
      );
      const scopes = session.kernel.command<number[]>(KernelOp.ScopesForOwner, {
        owner: { kind: OwnerKind.Object, id: object.id },
      });
      await Promise.all(scopes.map((scope) => session.programs.activate(scope)));
      try {
        session.snapshot(object.id);
        object.binding = new StateBinding(session, object.id);
      } catch {
        /* State is optional. */
      }
    } else {
      const facet = session.facets.records.get(acquired.facet);
      if (!facet) refuse(Errors.Invalid.FacetGone);
      await session.facets.ready(facet);
      const environment = this.createEnvironment(object, facet);
      object.versions.set(facet.info.id, environment);
      await this.session.programs.activate(environment.id);
    }
    if (
      session.kernel.command<Status>(KernelOp.Status, { reference: acquired.reference }) ===
      Status.Loading
    )
      session.kernel.command(KernelOp.Ready, { object: object.id });
    object.live = true;
    session.lifecycle.flush();
  }

  /** Run a service factory synchronously and register its scope. */
  createEnvironment(object: ObjectRecord, facet: FacetRecord): Environment {
    const definition = facet.definition.services?.[object.address.service];
    if (!definition) refuse(Errors.Invalid.ServiceGone);
    const environment = this.environment({ kind: OwnerKind.Object, id: object.id }, facet, object);
    try {
      const value = definition.create?.(new Declaration(this.session, environment)) ?? {};
      if (value && typeof (value as { then?: unknown }).then === "function")
        refuse(Errors.Invalid.AsyncFactory, "Factories must be synchronous");
      environment.value = value;
      return environment;
    } finally {
      environment.phase = EnvironmentPhase.Prepared;
      this.session.programs.register(environment);
    }
  }

  /** Share host activation and reopen retired resource scopes. */
  readyHostScope(): Promise<void> {
    const session = this.session;
    if (session.closing) return Promise.reject(new TembraError(Errors.Closed.Host));
    if (this.#hostActivation) return this.#hostActivation;
    const activation = (async () => {
      if (session.hostScope.phase === EnvironmentPhase.Closed)
        session.hostScope = await this.redeclareHostScope(session.hostScope);
      await this.session.programs.activate(session.hostScope.id);
      while (this.#pendingHostScopes.size) {
        let environment = this.#pendingHostScopes.values().next().value!;
        if (
          environment.phase === EnvironmentPhase.Closed ||
          [...environment.resources.values()].some((resource) =>
            session.resources
              .dependencyIds(resource.options)
              .some((id, index) => id !== resource.dependencies[index]),
          )
        ) {
          const previous = environment;
          environment = await this.redeclareHostScope(previous);
          this.#pendingHostScopes.delete(previous);
          this.#pendingHostScopes.add(environment);
        }
        await this.session.programs.activate(environment.id);
        this.#pendingHostScopes.delete(environment);
      }
    })();
    this.#hostActivation = activation;
    const clear = () => {
      if (this.#hostActivation === activation) this.#hostActivation = undefined;
    };
    activation.then(clear, clear);
    return activation;
  }

  /** Clean up the previous scope before rebinding its resources. */
  private async redeclareHostScope(previous: Environment): Promise<Environment> {
    const session = this.session;
    await session.programs.cleanup([previous]);
    const environment = this.environment({ kind: OwnerKind.Host }, previous.facet);
    session.resources.redeclare(environment, previous);
    environment.phase = EnvironmentPhase.Prepared;
    session.programs.register(environment);
    return environment;
  }

  /** Resolve a declared dependency and its allowed methods. */
  requirement(
    environment: Environment,
    service: string,
  ): { service: string; apiVersion?: number; methods?: readonly string[] } {
    const own = environment.facet.definition.services?.[service];
    if (own) return { service, apiVersion: environment.facet.info.apiVersion };
    for (const [alias, requirement] of Object.entries(
      environment.facet.definition.requires ?? {},
    )) {
      let provider: KernelFacet;
      try {
        provider = this.session.kernel.command<KernelFacet>(KernelOp.FacetNamed, {
          name: requirement.facet ?? alias,
          ticket: environment.facet.ticket,
        });
      } catch (error) {
        if (
          error instanceof TembraError &&
          error.code === Code.Gone &&
          error.reason === Reason.Facet
        )
          continue;
        throw error;
      }
      const selected =
        alias === service
          ? (provider.services.find((s) => s.name === alias) ??
            (provider.services.length === 1 ? provider.services[0] : undefined))
          : provider.services.find((s) => s.name === service);
      if (!selected) continue;
      const methods = requirement.services?.[selected.name] ?? requirement.methods;
      if (!methods) throw new TembraError(Errors.Forbidden.UndeclaredService, service);
      return { service: selected.name, apiVersion: requirement.apiVersion, methods };
    }
    throw new TembraError(Errors.Forbidden.UndeclaredDependency, service);
  }

  /** Require a named resource declared by this host. */
  hostResource<T>(name: string): Resource<T> {
    const resource = this.#hostResources.get(name);
    if (!resource) throw new TembraError(Errors.Forbidden.HostResource, name);
    return resource.handle;
  }

  /** Add a host-owned resource and schedule its activation. */
  resource<T>(name: string, options: ResourceOptions<T>): Resource<T> {
    const session = this.session;
    if (session.closing) throw new TembraError(Errors.Closed.Host);
    if (this.#hostResources.has(name)) refuse(Errors.Invalid.DuplicateResource);
    const environment = this.environment({ kind: OwnerKind.Host }, session.hostScope.facet);
    try {
      const resource = new Declaration(session, environment).resource(name, options);
      this.#hostResources.set(name, environment.resources.get(name)!);
      environment.phase = EnvironmentPhase.Prepared;
      session.programs.register(environment);
      this.#pendingHostScopes.add(environment);
      session.track(this.readyHostScope()).catch((error) => session.report(error));
      return resource;
    } catch (error) {
      session.programs.scopes.delete(environment.id);
      throw error;
    }
  }

  /** Index resources declared by the initial host scope. */
  registerHost(environment: Environment): void {
    for (const [name, resource] of environment.resources) this.#hostResources.set(name, resource);
  }

  /** Drop host resource indexes during shutdown. */
  clear(): void {
    this.#hostResources.clear();
    this.#pendingHostScopes.clear();
  }
}
