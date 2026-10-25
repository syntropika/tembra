import { Errors } from "../errors.ts";
import { KernelOp } from "../protocol.ts";
import { Reason, ReloadStatus } from "../identifiers.ts";
import { TembraError } from "../error.ts";
import type { Reload } from "../kernel.ts";
import type { FacetDefinition, PreparedReload, ReloadOptions, ReloadReport } from "../types.ts";
import type { FacetRecord } from "./records.ts";
import type { Session } from "./session.ts";

/** Captures JS factories and exposes the engine's transaction handle. No reload policy lives here. */
export class Replacements {
  #pending: Promise<PreparedReload> | undefined;
  #program: number | undefined;
  #prepared: PreparedReload | undefined;
  constructor(readonly session: Session) {}

  async replace(
    definitions: readonly FacetDefinition[],
    options: ReloadOptions = {},
    remove: readonly string[] = [],
  ): Promise<ReloadReport> {
    return (await this.prepare(definitions, options, remove)).commit();
  }
  prepare(
    definitions: readonly FacetDefinition[],
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
  async cancel(): Promise<void> {
    if (this.#prepared?.status === ReloadStatus.Prepared) return this.#prepared.abort();
    if (this.#program !== undefined) {
      this.session.kernel.command(KernelOp.ProgramAbort, { program: this.#program });
      const settled = this.session.programs.run(this.#program);
      this.session.flush();
      await settled.catch(() => {});
    }
    await this.#pending?.catch(() => undefined);
  }

  private async capture(
    definitions: readonly FacetDefinition[],
    options: ReloadOptions,
    remove: readonly string[],
  ): Promise<PreparedReload> {
    const session = this.session;
    const declarations = session.facets.declarations(definitions);
    const reload = session.kernel.command<Reload>(KernelOp.Prepare, {
      facets: declarations.metadata,
      remove,
    });
    const definitionsByName = new Map(
      declarations.definitions.map((definition) => [definition.name, definition]),
    );
    const records: FacetRecord[] = reload.facets.map((info) => ({
      info,
      definition: definitionsByName.get(info.name)!,
      ticket: reload.id,
    }));
    const recordsById = new Map(records.map((record) => [record.info.id, record]));
    let program: number | undefined;
    try {
      for (const record of records) session.facets.scope(record);
      for (const id of reload.objects) {
        const object = session.objects.get(id);
        if (!object) continue;
        const facet = reload.providers[object.address.service];
        const record = facet === undefined ? undefined : recordsById.get(facet);
        if (record) session.createEnvironment(object, record);
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
        const scopes = reload.facets.flatMap((facet) =>
          session.kernel.command<number[]>(KernelOp.ScopesForFacet, { facet: facet.id }),
        );
        await session.programs.cleanup(scopes.map((scope) => session.programs.scopes.get(scope)!));
      }
      throw error;
    }
    this.#program = undefined;
    const id = program;
    const report = {
      codeVersion: reload.codeVersion,
      facets: records.map((record) => record.info.name),
    };
    let status: PreparedReload["status"] = ReloadStatus.Prepared;
    return (this.#prepared = {
      report,
      get status() {
        return status;
      },
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
