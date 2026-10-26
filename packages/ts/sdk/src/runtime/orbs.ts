import { Declaration } from "../authoring/declaration.ts";
import { orb } from "../authoring/orb.ts";
import type { ContributionHandler, OrbDefinition, Participant } from "../authoring/types.ts";
import { Lifetime } from "../bridge/identifiers.ts";
import type { KernelOrb, Reload } from "../bridge/kernel.ts";
import { EnvironmentPhase, KernelOp, OwnerKind } from "../bridge/protocol.ts";
import { refuse, TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import { Context, context as checkedContext } from "./context.ts";
import type { Environment, OrbRecord } from "./records.ts";
import type { Session } from "./session.ts";

export class Orbs {
  readonly records = new Map<number, OrbRecord>();
  readonly operations = new Map<number, { orb: OrbRecord; handler: ContributionHandler }>();
  #ready = new Map<number, Promise<void>>();
  #ordinals = new Map<string, number>();
  #nextOrdinal = 0;

  /** Retain local implementations and contribution callbacks. */
  constructor(readonly session: Session) {}

  /** Capture definitions and encode their kernel-owned contracts. */
  declarations(definitions: readonly OrbDefinition[]): {
    definitions: OrbDefinition[];
    metadata: unknown[];
  } {
    const captured = definitions.map((definition) => orb(definition));
    const current = this.session.kernel.command<KernelOrb[]>(KernelOp.Orbs);
    const lookup = (name: string) => captured.find((f) => f.name === name);
    const metadata = captured.map((definition) => {
      if (
        !this.session.kernel.features.views &&
        Object.values(definition.services ?? {}).some((service) => service.access)
      )
        throw new TembraError(Errors.Unsupported.Views);
      const requires = Object.entries(definition.requires ?? {}).map(([alias, requirement]) => {
        const name = requirement.orb ?? alias;
        const provider = lookup(name);
        const info = current.find((f) => f.name === name);
        const names = provider
          ? Object.keys(provider.services ?? {})
          : (info?.services.map((s) => s.name) ?? []);
        const service = names.includes(alias) ? alias : names.length === 1 ? names[0] : undefined;
        if (requirement.methods && !service)
          refuse(Errors.Invalid.DependencyService, `Use explicit services for ${name}`);
        return {
          orb: name,
          apiVersion: requirement.apiVersion,
          services:
            requirement.services ??
            (requirement.methods ? { [service!]: requirement.methods } : {}),
          hooks: requirement.hooks ?? [],
        };
      });
      for (const [alias, hooks] of Object.entries(definition.contributes ?? {})) {
        const requirement = definition.requires?.[alias];
        if (!requirement) refuse(Errors.Invalid.HookDependency, alias);
        for (const [hook, contribution] of Object.entries(hooks)) {
          if (!requirement.hooks?.includes(hook)) refuse(Errors.Invalid.HookDependency, hook);
          const priority = typeof contribution === "function" ? 0 : (contribution.priority ?? 0);
          if (!Number.isInteger(priority) || priority < -0x8000_0000 || priority > 0x7fff_ffff)
            refuse(Errors.Invalid.Priority);
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
          lifetime: service.lifetime ?? Lifetime.Referenced,
          linger: service.linger ?? 0,
        })),
      };
    });
    return { definitions: captured, metadata };
  }

  /** Commit new orb metadata before opening its scopes. */
  async install(definitions: readonly OrbDefinition[]): Promise<void> {
    const current = this.session.kernel.command<KernelOrb[]>(KernelOp.Orbs);
    if (definitions.some((f) => current.some((installed) => installed.name === f.name)))
      refuse(Errors.Invalid.OrbInstalled, "Use reload to replace an installed orb");
    const prepared = this.declarations(definitions);
    const reload = this.session.kernel.command<Reload>(KernelOp.Prepare, {
      orbs: prepared.metadata,
    });
    this.session.kernel.command(KernelOp.Commit, { ticket: reload.id });
    const records = reload.orbs.map((info) => ({
      info,
      definition: prepared.definitions.find((f) => f.name === info.name)!,
    }));
    for (const record of records) this.records.set(record.info.id, record);
    for (const record of records) this.registerContributions(record);
    await Promise.all(records.map((record) => this.ready(record)));
    for (const record of records) {
      for (const service of record.info.services.filter((s) => s.lifetime === Lifetime.Host)) {
        const reference = await this.session.handles.get(service.name, "main", {
          principal: "host",
        });
        reference.release();
      }
    }
    this.session.lifecycle.flush();
  }

  /** Declare one shared scope for this orb version. */
  scope(record: OrbRecord): Environment {
    if (record.scope) return record.scope;
    const environment = this.session.scopes.environment(
      { kind: OwnerKind.Orb, id: record.info.owner },
      record,
    );
    record.scope = environment;
    try {
      const value = record.definition.create?.(new Declaration(this.session, environment)) ?? {};
      if (value && typeof (value as { then?: unknown }).then === "function")
        refuse(Errors.Invalid.AsyncFactory);
      environment.value = value;
    } finally {
      environment.phase = EnvironmentPhase.Prepared;
      this.session.programs.register(environment);
    }
    return environment;
  }

  /** Share pending activation and clean up failed orb scopes. */
  ready(record: OrbRecord): Promise<void> {
    if (record.scope?.phase === EnvironmentPhase.Live) return Promise.resolve();
    let ready = this.#ready.get(record.info.id);
    if (!ready) {
      ready = (async () => {
        try {
          await this.session.scopes.readyHostScope();
          if (record.scope?.phase === EnvironmentPhase.Closed) delete record.scope;
          const scope = this.scope(record);
          await this.session.programs.activate(scope.id);
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

  /** Register callback identities with stable contribution order. */
  registerContributions(record: OrbRecord): void {
    for (const [alias, hooks] of Object.entries(record.definition.contributes ?? {})) {
      const requirement = record.definition.requires![alias]!;
      const target = requirement.orb ?? alias;
      for (const [hook, contribution] of Object.entries(hooks)) {
        const operation = this.session.kernel.command<number>(KernelOp.Allocate);
        const key = JSON.stringify([record.info.name, target, hook]);
        if (!this.#ordinals.has(key)) this.#ordinals.set(key, this.#nextOrdinal++);
        this.operations.set(operation, {
          orb: record,
          handler: typeof contribution === "function" ? contribution : contribution.handle,
        });
        this.session.kernel.command(KernelOp.Contribute, {
          orb: record.info.id,
          target,
          hook,
          operation,
          priority: typeof contribution === "function" ? 0 : (contribution.priority ?? 0),
          ordinal: this.#ordinals.get(key),
        });
      }
    }
  }

  /** Validate the caller and execute the kernel's admitted hook plan. */
  async runHook(
    environment: Environment,
    name: string,
    context: Context,
    input: unknown,
  ): Promise<unknown> {
    context = checkedContext(context, this.session);
    const definition = environment.orb.definition.hooks?.[name];
    if (!definition) refuse(Errors.Invalid.Hook, name);
    const plan = this.session.kernel.command<number[]>(KernelOp.HookPlan, {
      root: context.root,
      target: environment.orb.info.name,
      hook: name,
    });
    const participants: Participant[] = plan.map((operation) => async (ctx, input, next) => {
      const contributor = this.operations.get(operation);
      if (!contributor) throw new TembraError(Errors.Gone.Contribution);
      const scope = contributor.orb.scope;
      if (!scope) throw new TembraError(Errors.Unavailable.OrbScope);
      return this.session.calls.owned(
        scope,
        operation,
        (entered) => contributor.handler(entered, input, next),
        ctx,
      );
    });
    return definition.run(context, input, participants);
  }

  /** Clean up a retired version before removing its callbacks. */
  retire(id: number): void {
    const scopes = this.session.kernel.command<number[]>(KernelOp.ScopesForOrb, { orb: id });
    const cleanup = this.session.programs.cleanupScopes(scopes);
    this.session.track(
      cleanup.then(() => {
        for (const [operation, contribution] of this.operations)
          if (contribution.orb.info.id === id) this.operations.delete(operation);
        this.records.delete(id);
        this.#ready.delete(id);
      }),
      true,
    );
  }
}
