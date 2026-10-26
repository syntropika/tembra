import type {
  OrbDefinition,
  PreparedReload,
  ReloadOptions,
  ReloadReport,
} from "../authoring/types.ts";
import { Reason, ReloadStatus } from "../bridge/identifiers.ts";
import type { Reload } from "../bridge/kernel.ts";
import { KernelOp } from "../bridge/protocol.ts";
import { TembraError } from "../error.ts";
import { Errors } from "../errors.ts";
import type { OrbRecord } from "./records.ts";
import type { Session } from "./session.ts";

/** Captures JS factories and exposes the engine's transaction handle. No reload policy lives here. */
export class Replacements {
  #pending: Promise<PreparedReload> | undefined;
  #program: number | undefined;
  #prepared: PreparedReload | undefined;

  /** Track one replacement candidate for this session. */
  constructor(readonly session: Session) {}

  /** Prepare and commit a replacement in one operation. */
  async replace(
    definitions: readonly OrbDefinition[],
    options: ReloadOptions = {},
    remove: readonly string[] = [],
  ): Promise<ReloadReport> {
    return (await this.prepare(definitions, options, remove)).commit();
  }

  /** Reject overlapping replacement attempts before capturing callbacks. */
  prepare(
    definitions: readonly OrbDefinition[],
    options: ReloadOptions = {},
    remove: readonly string[] = [],
  ): Promise<PreparedReload> {
    if (this.session.closing) return Promise.reject(new TembraError(Errors.Closed.Host));
    if (this.#pending) return Promise.reject(new TembraError(Errors.Conflict.ReloadRunning));
    const pending = this.capture(definitions, options, remove);
    this.#pending = pending;
    pending.catch(() => {
      if (this.#pending === pending) this.#pending = undefined;
    });
    return pending;
  }

  /** Abort pending preparation and wait for physical recovery. */
  async cancel(): Promise<void> {
    if (this.#prepared?.status === ReloadStatus.Prepared) return this.#prepared.abort();
    if (this.#program !== undefined) {
      this.session.kernel.command(KernelOp.ProgramAbort, { program: this.#program });
      const settled = this.session.programs.run(this.#program);
      this.session.lifecycle.flush();
      await settled.catch(() => {});
    }
    await this.#pending?.catch(() => undefined);
  }

  /** Bind candidate callbacks to the kernel's prepared replacement. */
  private async capture(
    definitions: readonly OrbDefinition[],
    options: ReloadOptions,
    remove: readonly string[],
  ): Promise<PreparedReload> {
    const session = this.session;
    const declarations = session.orbs.declarations(definitions);
    const reload = session.kernel.command<Reload>(KernelOp.Prepare, {
      orbs: declarations.metadata,
      remove,
    });
    const definitionsByName = new Map(
      declarations.definitions.map((definition) => [definition.name, definition]),
    );
    const records: OrbRecord[] = reload.orbs.map((info) => ({
      info,
      definition: definitionsByName.get(info.name)!,
      ticket: reload.id,
    }));
    const recordsById = new Map(records.map((record) => [record.info.id, record]));
    let program: number | undefined;
    try {
      for (const record of records) session.orbs.scope(record);
      for (const id of reload.objects) {
        const object = session.objects.get(id);
        if (!object) continue;
        const orb = reload.providers[object.address.service];
        const record = orb === undefined ? undefined : recordsById.get(orb);
        if (record) session.scopes.createEnvironment(object, record);
      }
      for (const record of records) record.ticket = undefined;
      program = session.kernel.command<number>(KernelOp.ScopesReloadRegistered, {
        ticket: reload.id,
        deadline: options.deadline ?? 5000,
        now: session.now(),
      });
      this.#program = program;
      await session.programs.run(program);
      // Preparation may have settled just before close; observe physical abort before returning.
      if (session.closing) await session.programs.run(program);
    } catch (error) {
      this.#program = undefined;
      if (program === undefined) {
        session.kernel.command(KernelOp.Abort, { ticket: reload.id });
        const scopes = reload.orbs.flatMap((orb) =>
          session.kernel.command<number[]>(KernelOp.ScopesForOrb, { orb: orb.id }),
        );
        await session.programs.cleanupScopes(scopes);
      }
      throw error;
    }
    this.#program = undefined;
    const id = program;
    const report = {
      codeVersion: reload.codeVersion,
      orbs: records.map((record) => record.info.name),
    };
    let status: PreparedReload["status"] = ReloadStatus.Prepared;
    return (this.#prepared = {
      report,
      /** Read whether this prepared candidate can still commit or abort. */
      get status() {
        return status;
      },
      /** Publish a prepared candidate once, then await its activation. */
      commit: async () => {
        if (status !== ReloadStatus.Prepared) throw new TembraError(Errors.Conflict.ReloadTicket);
        this.#prepared = undefined;
        try {
          session.kernel.command(KernelOp.ProgramCommit, { program: id });
        } catch (error) {
          session.kernel.command(KernelOp.ProgramAbort, { program: id });
          await session.programs.run(id).catch(() => {});
          status = ReloadStatus.Aborted;
          this.#pending = undefined;
          throw error;
        }
        status = ReloadStatus.Committed;
        this.#pending = undefined;
        await session.programs.run(id);
        return report;
      },
      /** Discard an uncommitted candidate and await physical recovery. */
      abort: async () => {
        if (status === ReloadStatus.Aborted) return;
        if (status !== ReloadStatus.Prepared)
          throw new TembraError(Errors.Conflict.AlreadyCommitted);
        status = ReloadStatus.Aborted;
        this.#prepared = undefined;
        this.#pending = undefined;
        session.kernel.command(KernelOp.ProgramAbort, { program: id });
        try {
          await session.programs.run(id);
        } catch (error) {
          if (!(error instanceof TembraError) || error.reason !== Reason.ReloadAborted) throw error;
        }
      },
    });
  }
}
