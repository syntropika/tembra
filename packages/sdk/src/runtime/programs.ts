import { failure, TembraError } from "../error.ts";
import { validate } from "../state.ts";
import { deferred } from "./records.ts";
import type { Action, Effect } from "../kernel.ts";
import type { Environment, ResourceRecord } from "./records.ts";
import type { Session } from "./session.ts";

/** Language effects only. The Rust program owns readiness, ordering, gates and recovery decisions. */
export class Programs {
  readonly scopes = new Map<number, Environment>();
  #pending = new Map<number, ReturnType<typeof deferred<"prepared" | "complete">>>();
  constructor(readonly session: Session) {}

  register(environment: Environment): void {
    if (this.session.kernel.command<boolean>("scope-exists", { scope: environment.id })) return;
    const parent =
      environment.owner.kind === "host"
        ? undefined
        : environment.object
          ? environment.facet.scope
          : this.session.hostScope;
    this.session.kernel.command("scope-register", {
      scope: environment.id,
      owner: environment.owner,
      facet: environment.facet.info.id,
      parents: parent ? [parent.id] : [],
      dependencies: [...environment.dependencies.values()].map(
        (reference) => this.session.reference(reference).id,
      ),
      resources: [...environment.resources.values()].map((resource) => resource.id),
      stateVersion: environment.state?.options.version ?? (environment.state ? 1 : undefined),
      migrate: Boolean(environment.state?.options.migrate),
    });
  }

  activate(environment: Environment): Promise<void> {
    this.register(environment);
    return this.run(
      this.session.kernel.command<number>("scope-activate", { scope: environment.id }),
    ).then(() => {});
  }

  cleanup(environments: readonly Environment[], object?: number): Promise<void> {
    const scopes = environments
      .map((environment) => environment.id)
      .filter((scope) => this.session.kernel.command<boolean>("scope-exists", { scope }));
    if (!scopes.length && object === undefined) return Promise.resolve();
    return this.run(this.session.kernel.command<number>("scopes-cleanup", { scopes, object })).then(
      () => {},
    );
  }

  run(program: number): Promise<"prepared" | "complete"> {
    const pending = this.#pending.get(program);
    if (pending) return pending.promise;
    const completion = deferred<"prepared" | "complete">();
    this.#pending.set(program, completion);
    try {
      this.session.kernel.command("program-run", { program });
      this.session.flush();
    } catch (error) {
      this.#pending.delete(program);
      completion.reject(error);
    }
    return completion.promise;
  }

  /** Dispatches opaque engine requests and acknowledges physical callback settlement. */
  dispatch(action: Action): void {
    if (action.type === "execute") {
      this.session.track(
        Promise.allSettled(action.effects!.map((effect) => this.execute(effect))).then(
          (settled) => {
            const results = settled.map((result) => {
              if (result.status === "fulfilled") return { value: result.value ?? null };
              const { code, reason, message } = failure(result.reason);
              return { error: { code, reason, message } };
            });
            this.session.kernel.command("program-complete-step", { program: action.id, results });
            this.session.flush();
          },
        ),
      );
    } else if (action.type === "cancel-effects") {
      for (const effect of action.effects!) {
        if (effect.type !== "open" && effect.type !== "restore") continue;
        const resource =
          this.session.resources.records.get(effect.resource!) ??
          this.session.resources.closedRecords.get(effect.resource!);
        if (resource) resource.controller.abort(new TembraError("cancelled", "engine_effect"));
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
        const error = new TembraError(action.error.code, action.error.reason, action.error.message);
        if (completion) completion.reject(error);
        else {
          this.session.cleanupErrors.push(error);
          this.session.report(error);
        }
      } else completion?.resolve(action.type === "program-prepared" ? "prepared" : "complete");
    }
  }

  private resource(id: number): ResourceRecord {
    const direct =
      this.session.resources.records.get(id) ?? this.session.resources.closedRecords.get(id);
    if (direct) return direct;
    throw new TembraError("gone", "resource");
  }

  private async execute(effect: Effect): Promise<number | void> {
    const environment = effect.scope === undefined ? undefined : this.scopes.get(effect.scope);
    if (effect.scope !== undefined && !environment && this.session.options.native)
      return this.session.options.native.effect(effect);
    if (
      (effect.type === "close" || effect.type === "open") &&
      !this.session.resources.records.has(effect.resource!) &&
      !this.session.resources.closedRecords.has(effect.resource!) &&
      this.session.options.native
    )
      return this.session.options.native.effect(effect);
    switch (effect.type) {
      case "open":
        await this.resource(effect.resource!).start();
        break;
      case "close":
        await this.session.resources.close(this.resource(effect.resource!));
        break;
      case "restore":
        await this.session.resources.reopen(this.resource(effect.previous!), effect.resource!);
        break;
      case "stop":
        if (environment) await this.session.activities.stop(environment, effect.keepListeners);
        break;
      case "prepare": {
        if (!environment) throw new TembraError("gone", "scope");
        this.session.activities.prepare(
          environment,
          effect.previous ? this.scopes.get(effect.previous) : undefined,
        );
        break;
      }
      case "validate":
        validate(this.session.value(effect.value!), environment!.state!.options);
        break;
      case "initialize":
        return this.candidate(environment!, environment!.state!.initial);
      case "migrate": {
        const state = environment!.state!;
        return this.candidate(
          environment!,
          state.options.migrate!(this.session.value(effect.value!), effect.version!),
        );
      }
      case "adopt": {
        const facets = new Map<number, Environment["facet"]>();
        for (const pair of effect.replacements!) {
          if (!pair.next) continue;
          const next = this.scopes.get(pair.next)!;
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
          if (next?.object) this.session.refreshReferences(next.object, next.facet);
        }
        for (const pair of effect.replacements!)
          if (pair.next) this.scopes.get(pair.next)?.state?.publish?.();
        break;
      }
      case "start":
        if (environment) {
          this.session.activities.start(environment);
          if (environment.object) environment.object.live = true;
        }
        break;
      case "drop": {
        if (!environment) break;
        for (const reference of environment.dependencies.values())
          this.session.revoke(this.session.reference(reference).id);
        environment.dependencies.clear();
        for (const resource of environment.resources.values())
          this.session.resources.closedRecords.delete(resource.id);
        environment.phase = "closed";
        if (environment.object) environment.object.versions.delete(environment.facet.info.id);
        this.scopes.delete(environment.id);
        break;
      }
      default:
        throw new TembraError("unsupported", "engine_effect", effect.type);
    }
  }
  private candidate(environment: Environment, candidate: unknown): number {
    const state = environment.state!;
    const value = validate(candidate, state.options);
    const selected = state.binding.prepare(value);
    const id = this.session.values.put(value);
    state.publish = () => this.session.publish(state.binding.adopt(selected, value));
    return id;
  }
}
