import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { KernelOp } from "../src/protocol.ts";
import { deferred } from "../src/runtime/records.ts";
import type { Declaration } from "../src/runtime/declaration.ts";

for (const reload of [false, true])
  test(`concurrent object events retain owner identities and access views${reload ? " after reload" : ""}`, async () => {
    const host = createHost({ principal: "system" });
    const callbacks = new Set<(value: number) => void>();
    const source = {
      subscribe(callback: (value: number) => void) {
        callbacks.add(callback);
        return () => {
          callbacks.delete(callback);
        };
      },
    };
    const arrived = deferred<void>();
    const resume = deferred<void>();
    const finished = deferred<void>();
    const roots = new Set<number>();
    const seen: {
      owner: string;
      before: string;
      after: string;
      profile: unknown;
      admin: boolean;
      version: number;
    }[] = [];
    const access: string[] = [];
    const listen = (self: Declaration, version: number) => {
      const owner = self.principal;
      self.on("incoming", source, async (ctx) => {
        const before = ctx.principal;
        roots.add(ctx.root);
        if (roots.size === 3) arrived.resolve();
        await resume.promise;
        const profile = await ctx.get("profile", "me");
        seen.push({
          owner,
          before,
          after: ctx.principal,
          profile: await profile.read(ctx),
          admin: typeof profile.admin === "function",
          version,
        });
        if (seen.length === 3) finished.resolve();
      });
      return {};
    };
    const consumer = (version: number) =>
      facet({
        name: "consumer",
        apiVersion: 1,
        requires: { profile: { apiVersion: 1, methods: ["read"] } },
        create: (self) => listen(self, version),
        services: { listener: { create: (self) => listen(self, version), methods: {} } },
      });
    await host.install([
      facet({
        name: "profile",
        apiVersion: 1,
        services: {
          profile: {
            create: (self) => self.principal,
            access(principal) {
              access.push(principal);
              return { methods: principal === "system" ? ["read", "admin"] : ["read"] };
            },
            methods: {
              read: (owner, ctx) => ({ owner, principal: ctx.principal }),
              admin: () => "admin",
            },
          },
        },
      }),
      consumer(1),
    ]);
    const references = await Promise.all(
      ["alice", "bob"].map((principal) => host.get("listener", "main", { principal })),
    );
    if (reload) await host.reload([consumer(2)]);
    expect(callbacks.size).toBe(3);
    const commands = spyOn(host.session.kernel, "command");
    try {
      for (const callback of callbacks) callback(1);
      await arrived.promise;
      expect(host.diagnostics()).toMatchObject({ roots: 3, calls: 3 });
      resume.resolve();
      await finished.promise;
      await Promise.allSettled([...host.session.jobs]);
      expect(seen.sort((a, b) => a.owner.localeCompare(b.owner))).toEqual(
        ["alice", "bob", "system"].map((owner) => ({
          owner,
          before: owner,
          after: owner,
          profile: { owner, principal: owner },
          admin: owner === "system",
          version: reload ? 2 : 1,
        })),
      );
      expect(access.sort()).toEqual(["alice", "bob", "system"]);
      expect(
        commands.mock.calls
          .filter(([op]) => op === KernelOp.Root)
          .map(([, fields]) => fields?.principal)
          .sort(),
      ).toEqual(["alice", "bob", "system"]);
      expect(host.errors()).toEqual([]);
    } finally {
      commands.mockRestore();
      resume.resolve();
      for (const reference of references) reference.release();
      await host.close();
    }
    expect(callbacks.size).toBe(0);
    expect(host.diagnostics()).toMatchObject({ objects: 0, roots: 0, calls: 0, pendingJobs: 0 });
  });
