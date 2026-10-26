import { Declaration } from "../authoring/declaration.ts";
import type { HostOptions } from "../authoring/types.ts";
import type { Invocation, Snapshot } from "../bridge/kernel.ts";
import { Kernel } from "../bridge/kernel.ts";
import { EnvironmentPhase, KernelOp, OwnerKind } from "../bridge/protocol.ts";
import { jsonCodec } from "../codecs/json.ts";
import { failure, refuse, TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import type { StateBackend } from "../state.ts";
import { Telemetry } from "../telemetry.ts";
import { Values } from "../values.ts";
import { Activities } from "./activities.ts";
import { Calls } from "./calls.ts";
import { context } from "./context.ts";
import { Orbs } from "./orbs.ts";
import { Lifecycle } from "./lifecycle.ts";
import { Programs } from "./programs.ts";
import type { Environment, OrbRecord, ObjectRecord, ReferenceRecord } from "./records.ts";
import { References } from "./references.ts";
import { Replacements } from "./reload.ts";
import { Resources } from "./resources.ts";
import { Scopes } from "./scopes.ts";

/** Shared JS values and callbacks. Rust owns lifecycle policy. */
export class Session implements StateBackend {
  readonly contextTokens = new WeakMap<object, { root: number; source: number }>();
  readonly calls = new Calls(this);
  readonly handles = new References(this);
  readonly scopes = new Scopes(this);
  readonly lifecycle = new Lifecycle(this);
  readonly kernel: Kernel;
  readonly values: Values;
  readonly resources = new Resources(this);
  readonly activities = new Activities(this);
  readonly orbs = new Orbs(this);
  readonly replacements = new Replacements(this);
  readonly programs = new Programs(this);
  readonly objects = new Map<number, ObjectRecord>();
  readonly references = new Map<number, ReferenceRecord>();
  readonly rootReferences = new Map<number, Set<number>>();
  readonly controllers = new Map<number, AbortController>();
  readonly jobs = new Set<Promise<unknown>>();
  readonly cleanupErrors: unknown[] = [];
  hostScope: Environment;
  readonly codec;
  readonly name: string;
  readonly principal: string;
  readonly errors: TembraError[] = [];
  readonly now: () => number;
  readonly telemetry: Telemetry | undefined;
  closing = false;

  #publishing = false;

  #publications: (() => void)[] = [];

  /** Create callback storage around one Rust lifecycle kernel. */
  constructor(readonly options: HostOptions) {
    this.kernel = new Kernel(options.kernel);
    if (options.telemetry && !this.kernel.features.telemetry)
      throw new TembraError(Errors.Unsupported.Telemetry);
    this.telemetry = options.telemetry
      ? new Telemetry(options.telemetry.capacity, options.telemetry.sampleEvery)
      : undefined;
    this.values = new Values(() => this.kernel.command<number>(KernelOp.Allocate));
    this.codec = options.serialization ?? jsonCodec;
    this.name = options.name ?? "host";
    this.principal = options.principal ?? "host";
    const clock = options.now ?? (() => performance.now());
    const origin = options.clockOrigin ?? clock();
    this.now = () => Math.floor(clock() - origin);
    const hostOrb: OrbRecord = {
      info: {
        id: 0,
        owner: 0,
        name: "$host",
        apiVersion: 1,
        codeVersion: 1,
        services: [],
        hooks: [],
        requires: [],
      },
      definition: { name: "$host", apiVersion: 1 },
    };
    this.hostScope = this.scopes.environment({ kind: OwnerKind.Host }, hostOrb);
    for (const [name, declaration] of Object.entries(options.resources ?? {})) {
      new Declaration(this, this.hostScope).resource(name, declaration);
    }
    this.scopes.registerHost(this.hostScope);
    this.hostScope.phase = EnvironmentPhase.Prepared;
    this.programs.register(this.hostScope);
  }

  /** Read the object owner, or use the host identity. */
  principalFor(object: number): string {
    return this.objects.get(object)?.address.principal ?? this.principal;
  }

  /** Read the current value identity and state revision. */
  snapshot(object: number): Snapshot {
    return this.kernel.command(KernelOp.Snapshot, { object });
  }

  /** Resolve a local value or cache its native decoded value. */
  value<T>(id: number): T {
    if (this.values.has(id)) return this.values.get<T>(id);
    if (!this.options.native) refuse(Errors.Invalid.ValueGone);
    const value = this.options.native.value(id) as T;
    this.values.set(id, value);
    return value;
  }

  /** Require an admitted call for the target state owner. */
  check(value: unknown, object: number): Invocation {
    const ctx = context(value, this);
    if (!ctx.invocation || ctx.invocation.object !== object)
      throw new TembraError(Errors.Forbidden.StateOwner);
    return ctx.invocation;
  }

  /** Publish candidate state only if its expected revision still matches. */
  adopt(object: number, value: unknown, revision: string, candidate: unknown): Snapshot {
    const invocation = this.check(value, object);
    const id = this.values.put(candidate);
    try {
      return this.kernel.command(KernelOp.StateAdopt, {
        call: invocation.id,
        object,
        revision,
        value: id,
      });
    } catch (error) {
      this.values.drop(id);
      throw error;
    } finally {
      this.lifecycle.flush();
    }
  }

  /** Serialize notifications so reentrant updates preserve publication order. */
  publish(operation: () => void): void {
    this.#publications.push(operation);
    if (this.#publishing) return;
    this.#publishing = true;
    try {
      while (this.#publications.length) this.#publications.shift()!();
    } finally {
      this.#publishing = false;
    }
  }

  /** Retain a bounded error history without disrupting state adoption. */
  report(error: unknown): void {
    const reported = failure(error);
    if (this.errors.length === 64) this.errors.shift();
    this.errors.push(reported);
    try {
      this.options.onError?.(reported);
    } catch {
      /* Error reporting cannot invalidate an adopted state. */
    }
  }

  /** Retain physical work until settlement, including cancelled callers. */
  track<T>(job: Promise<T>, cleanup = false): Promise<T> {
    this.jobs.add(job);
    job.then(
      () => {
        this.jobs.delete(job);
      },
      (error) => {
        this.jobs.delete(job);
        if (cleanup) {
          this.cleanupErrors.push(error);
          this.report(error);
        }
      },
    );
    return job;
  }
}
