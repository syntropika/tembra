import { facet } from "../facet.ts";
import { refuse, TembraError } from "../error.ts";
import type { KernelFacet, Reload } from "../kernel.ts";
import type { FacetDefinition, ContributionHandler, Participant } from "../types.ts";
import type { Context } from "./context.ts";
import { Declaration } from "./declaration.ts";
import type { Environment, FacetRecord } from "./records.ts";
import type { Session } from "./session.ts";

export class Facets {
  readonly records = new Map<number, FacetRecord>();
  readonly operations = new Map<number, { facet: FacetRecord; handler: ContributionHandler }>();
  #ready = new Map<number, Promise<void>>();
  #ordinals = new Map<string, number>();
  #nextOrdinal = 0;
  constructor(readonly session: Session) {}

  declarations(definitions: readonly FacetDefinition[]): {
    definitions: FacetDefinition[];
    metadata: unknown[];
  } {
    const captured = definitions.map((definition) => facet(definition));
    const current = this.session.kernel.command<KernelFacet[]>("facets");
    const lookup = (name: string) => captured.find((f) => f.name === name);
    const metadata = captured.map((definition) => {
      if (
        !this.session.kernel.features.views &&
        Object.values(definition.services ?? {}).some((service) => service.access)
      )
        throw new TembraError("unsupported", "views");
      const requires = Object.entries(definition.requires ?? {}).map(([alias, requirement]) => {
        const name = requirement.facet ?? alias;
        const provider = lookup(name);
        const info = current.find((f) => f.name === name);
        const names = provider
          ? Object.keys(provider.services ?? {})
          : (info?.services.map((s) => s.name) ?? []);
        const service = names.includes(alias) ? alias : names.length === 1 ? names[0] : undefined;
        if (requirement.methods && !service)
          refuse("dependency_service", `Use explicit services for ${name}`);
        return {
          facet: name,
          apiVersion: requirement.apiVersion,
          services:
            requirement.services ??
            (requirement.methods ? { [service!]: requirement.methods } : {}),
          hooks: requirement.hooks ?? [],
        };
      });
      for (const [alias, hooks] of Object.entries(definition.contributes ?? {})) {
        const requirement = definition.requires?.[alias];
        if (!requirement) refuse("hook_dependency", alias);
        for (const [hook, contribution] of Object.entries(hooks)) {
          if (!requirement.hooks?.includes(hook)) refuse("hook_dependency", hook);
          const priority = typeof contribution === "function" ? 0 : (contribution.priority ?? 0);
          if (!Number.isInteger(priority) || priority < -0x8000_0000 || priority > 0x7fff_ffff)
            refuse("priority");
        }
      }
      return {
        name: definition.name,
        apiVersion: definition.apiVersion,
        requires,
        hooks: Object.keys(definition.hooks ?? {}),
        services: Object.entries(definition.services ?? {}).map(([name, service]) => ({
          name,
          methods: Object.keys(service.methods),
          lifetime: service.lifetime ?? "referenced",
          linger: service.linger ?? 0,
        })),
      };
    });
    return { definitions: captured, metadata };
  }

  async install(definitions: readonly FacetDefinition[]): Promise<void> {
    const current = this.session.kernel.command<KernelFacet[]>("facets");
    if (definitions.some((f) => current.some((installed) => installed.name === f.name)))
      refuse("facet_installed", "Use reload to replace an installed facet");
    const prepared = this.declarations(definitions);
    const reload = this.session.kernel.command<Reload>("prepare", { facets: prepared.metadata });
    this.session.kernel.command("commit", { ticket: reload.id });
    const records = reload.facets.map((info) => ({
      info,
      definition: prepared.definitions.find((f) => f.name === info.name)!,
    }));
    for (const record of records) this.records.set(record.info.id, record);
    for (const record of records) this.registerContributions(record);
    await Promise.all(records.map((record) => this.ready(record)));
    for (const record of records) {
      for (const service of record.info.services.filter((s) => s.lifetime === "host")) {
        const reference = await this.session.get(service.name, "main", { principal: "host" });
        reference.release();
      }
    }
    this.session.flush();
  }

  scope(record: FacetRecord): Environment {
    if (record.scope) return record.scope;
    const environment = this.session.environment({ kind: "facet", id: record.info.owner }, record);
    record.scope = environment;
    try {
      const value = record.definition.create?.(new Declaration(this.session, environment)) ?? {};
      if (value && typeof (value as { then?: unknown }).then === "function")
        refuse("async_factory");
      environment.value = value;
    } finally {
      environment.phase = "prepared";
      this.session.programs.register(environment);
    }
    return environment;
  }

  ready(record: FacetRecord): Promise<void> {
    if (record.scope?.phase === "live") return Promise.resolve();
    let ready = this.#ready.get(record.info.id);
    if (!ready) {
      ready = (async () => {
        try {
          await this.session.readyHostScope();
          if (record.scope?.phase === "closed") delete record.scope;
          const scope = this.scope(record);
          await this.session.openEnvironment(scope);
        } catch (error) {
          if (record.scope) {
            await this.session.programs.cleanup([record.scope]);
            delete record.scope;
          }
          throw error;
        }
      })();
      this.#ready.set(record.info.id, ready);
      const clear = () => {
        if (this.#ready.get(record.info.id) === ready) this.#ready.delete(record.info.id);
      };
      ready.then(clear, clear);
    }
    return ready;
  }

  registerContributions(record: FacetRecord): void {
    for (const [alias, hooks] of Object.entries(record.definition.contributes ?? {})) {
      const requirement = record.definition.requires![alias]!;
      const target = requirement.facet ?? alias;
      for (const [hook, contribution] of Object.entries(hooks)) {
        const operation = this.session.kernel.command<number>("allocate");
        const key = JSON.stringify([record.info.name, target, hook]);
        if (!this.#ordinals.has(key)) this.#ordinals.set(key, this.#nextOrdinal++);
        this.operations.set(operation, {
          facet: record,
          handler: typeof contribution === "function" ? contribution : contribution.handle,
        });
        this.session.kernel.command("contribute", {
          facet: record.info.id,
          target,
          hook,
          operation,
          priority: typeof contribution === "function" ? 0 : (contribution.priority ?? 0),
          ordinal: this.#ordinals.get(key),
        });
      }
    }
  }

  async runHook(
    environment: Environment,
    name: string,
    context: Context,
    input: unknown,
  ): Promise<unknown> {
    const definition = environment.facet.definition.hooks?.[name];
    if (!definition) refuse("hook", name);
    const plan = this.session.kernel.command<number[]>("hook-plan", {
      root: context.root,
      target: environment.facet.info.name,
      hook: name,
    });
    const participants: Participant[] = plan.map((operation) => async (ctx, input, next) => {
      const contributor = this.operations.get(operation);
      if (!contributor) throw new TembraError("gone", "contribution");
      const scope = contributor.facet.scope;
      if (!scope) throw new TembraError("unavailable", "facet_scope");
      return this.session.owned(
        scope,
        operation,
        (entered) => contributor.handler(entered, input, next),
        ctx.root,
      );
    });
    return definition.run(context, input, participants);
  }

  retire(id: number): void {
    const scopes = this.session.kernel.command<number[]>("scopes-for-facet", { facet: id });
    const cleanup = scopes.length
      ? this.session.programs.run(this.session.kernel.command<number>("scopes-cleanup", { scopes }))
      : Promise.resolve();
    this.session.track(
      cleanup.then(() => {
        for (const [operation, contribution] of this.operations)
          if (contribution.facet.info.id === id) this.operations.delete(operation);
        this.records.delete(id);
        this.#ready.delete(id);
      }),
      true,
    );
  }
}
