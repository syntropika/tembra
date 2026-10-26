import { Code } from "../bridge/identifiers.ts";
import {
  ActionType,
  EffectType,
  EnvironmentPhase,
  KernelOp,
  OwnerKind,
} from "../bridge/protocol.ts";
import { Errors } from "../errors.ts";

import type { Action, Effect } from "../bridge/kernel.ts";
import { failure, TembraError } from "../error.ts";
import { validate } from "../state.ts";
import type { Environment, ResourceRecord } from "./records.ts";
import { deferred } from "./records.ts";
import type { Session } from "./session.ts";

/** Bound callback error text without emitting isolated UTF-16 surrogates. */
function errorText(value: string, limit: number): string {
  return value.slice(0, limit).replace(/[\uD800-\uDFFF]/gu, "\uFFFD");
}

/** Language effects only. The Rust program owns readiness, ordering, gates and recovery decisions. */
export class Programs {
  readonly scopes = new Map<number, Environment>();
  #pending = new Map<number, ReturnType<typeof deferred<void>>>();

  /** Retain effect scopes and pending program completions. */
  constructor(readonly session: Session) {}

  /** Describe a declared scope to the Rust lifecycle engine. */
  register(environment: Environment): void {
    const parent =
      environment.owner.kind === OwnerKind.Host
        ? undefined
        : environment.object
          ? environment.facet.scope
          : this.session.hostScope;
    this.session.kernel.command(KernelOp.ScopeRegister, {
      scope: environment.id,
      owner: environment.owner,
      facet: environment.facet.info.id,
      parents: parent ? [parent.id] : [],
      dependencies: [...environment.dependencies.values()].map(
        (reference) => this.session.handles.reference(reference).id,
      ),
      resources: [...environment.resources.values()].map((resource) => resource.id),
      stateVersion: environment.state?.options.version ?? (environment.state ? 1 : undefined),
      migrate: Boolean(environment.state?.options.migrate),
    });
  }

  /** Activate an already registered scope and wait for its program. */
  activate(scope: number): Promise<void> {
    return this.run(this.session.kernel.command<number>(KernelOp.ScopeActivate, { scope }));
  }

  /** Ask the kernel to retire existing scopes and await completion. */
  cleanup(environments: readonly Environment[]): Promise<void> {
    const scopes = environments
      .map((environment) => environment.id)
      .filter((scope) => this.session.kernel.command<boolean>(KernelOp.ScopeExists, { scope }));
    return this.cleanupScopes(scopes);
  }

  /** Run cleanup for kernel-issued scope identities without resolving JS callbacks. */
  cleanupScopes(scopes: readonly number[]): Promise<void> {
    return scopes.length
      ? this.run(this.session.kernel.command<number>(KernelOp.ScopesCleanup, { scopes }))
      : Promise.resolve();
  }

  /** Share one completion promise for a kernel program. */
  run(program: number): Promise<void> {
    const pending = this.#pending.get(program);
    if (pending) return pending.promise;
    const completion = deferred<void>();
    this.#pending.set(program, completion);
    try {
      this.session.kernel.command(KernelOp.ProgramRun, { program });
      this.session.lifecycle.flush();
    } catch (error) {
      this.#pending.delete(program);
      completion.reject(error);
    }
    return completion.promise;
  }

  /** Dispatches opaque engine requests and acknowledges physical callback settlement. */
  dispatch(action: Action): void {
    if (action.type === ActionType.Execute) {
      this.session.track(
        Promise.allSettled(action.effects!.map((effect) => this.execute(effect))).then(
          (settled) => {
            // Reserve fixed envelope overhead, then budget worst-case JSON escaping (6 bytes/code unit).
            const limit = Math.max(
              0,
              Math.min(
                2048,
                Math.floor((1024 * 1024 - 128 - settled.length * 96) / (12 * settled.length)),
              ),
            );
            const results = settled.map((result) => {
              if (result.status === "fulfilled") return { value: result.value ?? null };
              try {
                const { code, reason, message } = failure(result.reason);
                if (
                  !Object.values(Code).includes(code) ||
                  typeof reason !== "string" ||
                  typeof message !== "string"
                )
                  return { error: Errors.Unavailable.Callback };
                return {
                  error: {
                    code,
                    reason: errorText(reason, limit),
                    message: errorText(message, limit),
                  },
                };
              } catch {
                // Even a mutated Error with throwing getters must acknowledge physical settlement.
                return { error: Errors.Unavailable.Callback };
              }
            });
            this.session.kernel.command(KernelOp.ProgramCompleteStep, {
              program: action.id,
              results,
            });
            this.session.lifecycle.flush();
          },
        ),
      );
    } else if (action.type === ActionType.CancelEffects) {
      for (const effect of action.effects!) {
        if (effect.type !== EffectType.Open && effect.type !== EffectType.Restore) continue;
        const resource = this.session.resources.find(effect.resource!);
        if (resource) resource.controller.abort(new TembraError(Errors.Cancelled.EngineEffect));
        else if (this.session.options.native?.cancelEffect) {
          const cancel = this.session.options.native.cancelEffect.bind(this.session.options.native);
          this.session
            .track(Promise.resolve().then(() => cancel(effect)))
            .catch((error) => this.session.report(error));
        }
      }
    } else {
      const completion = this.#pending.get(action.id);
      this.#pending.delete(action.id);
      if (action.error) {
        const error = new TembraError(action.error);
        if (completion) completion.reject(error);
        else {
          this.session.cleanupErrors.push(error);
          this.session.report(error);
        }
      } else completion?.resolve();
    }
  }

  /** Resolve a live or recently closed resource callback record. */
  private resource(id: number): ResourceRecord {
    const direct = this.session.resources.find(id);
    if (direct) return direct;
    throw new TembraError(Errors.Gone.Resource);
  }

  /** Execute a language effect without deciding lifecycle order. */
  private async execute(effect: Effect): Promise<number | void> {
    const environment = effect.scope === undefined ? undefined : this.scopes.get(effect.scope);
    if (effect.scope !== undefined && !environment && this.session.options.native)
      return this.session.options.native.effect(effect);
    if (
      (effect.type === EffectType.Close || effect.type === EffectType.Open) &&
      !this.session.resources.find(effect.resource!) &&
      this.session.options.native
    )
      return this.session.options.native.effect(effect);
    switch (effect.type) {
      case EffectType.Open:
        await this.resource(effect.resource!).start();
        break;
      case EffectType.Close:
        await this.session.resources.close(this.resource(effect.resource!));
        break;
      case EffectType.Restore:
        await this.session.resources.reopen(this.resource(effect.previous!), effect.resource!);
        break;
      case EffectType.Stop:
        if (environment) await this.session.activities.stop(environment);
        break;
      case EffectType.Prepare: {
        if (!environment) throw new TembraError(Errors.Gone.Scope);
        this.session.activities.prepare(
          environment,
          effect.previous ? this.scopes.get(effect.previous) : undefined,
        );
        break;
      }
      case EffectType.Validate:
        validate(this.session.value(effect.value!), environment!.state!.options);
        break;
      case EffectType.Initialize:
        return this.candidate(environment!, environment!.state!.initial);
      case EffectType.Migrate: {
        const state = environment!.state!;
        return this.candidate(
          environment!,
          state.options.migrate!(this.session.value(effect.value!), effect.version!),
        );
      }
      case EffectType.Adopt: {
        const facets = new Map<number, Environment["facet"]>();
        for (const pair of effect.replacements!) {
          if (!pair.next) continue;
          const next = this.scopes.get(pair.next)!;
          for (const [resource, declaration] of next.resourceDeclarations ?? [])
            resource.options = declaration;
          delete next.resourceDeclarations;
          facets.set(next.facet.info.id, next.facet);
          if (next.object) {
            if (next.state) next.object.binding = next.state.binding;
            next.object.facet = next.facet.info.id;
            next.object.versions.set(next.facet.info.id, next);
          }
        }
        for (const record of facets.values()) {
          this.session.facets.records.set(record.info.id, record);
          this.session.facets.registerContributions(record);
        }
        for (const pair of effect.replacements!) {
          const next = pair.next ? this.scopes.get(pair.next) : undefined;
          if (next?.object) this.session.handles.refreshReferences(next.object, next.facet);
        }
        for (const pair of effect.replacements!)
          if (pair.next) this.scopes.get(pair.next)?.state?.publish?.();
        break;
      }
      case EffectType.Start:
        if (environment) {
          this.session.activities.start(environment);
          if (environment.object) environment.object.live = true;
        }
        break;
      case EffectType.Drop: {
        if (!environment) break;
        for (const reference of environment.dependencies.values())
          this.session.handles.revoke(this.session.handles.reference(reference).id);
        environment.dependencies.clear();
        for (const resource of environment.resources.values())
          this.session.resources.closedRecords.delete(resource.id);
        environment.phase = EnvironmentPhase.Closed;
        if (environment.object) environment.object.versions.delete(environment.facet.info.id);
        this.scopes.delete(environment.id);
        break;
      }
      default:
        throw new TembraError(Errors.Unsupported.EngineEffect, effect.type);
    }
  }

  /** Validate and retain candidate state before later publication. */
  private candidate(environment: Environment, candidate: unknown): number {
    const state = environment.state!;
    const value = validate(candidate, state.options);
    const id = this.session.values.put(value);
    state.publish = () =>
      this.session.publish(state.binding.adopt(state.binding.prepare(value), value));
    return id;
  }
}
