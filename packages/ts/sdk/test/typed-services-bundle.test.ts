import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import {
  Status,
  TembraError,
  type Context,
  type OrbDefinition,
  type ReferenceBase,
} from "../src/index.ts";
import { createHost } from "../src/node.ts";
import { build } from "../src/tooling/index.ts";

type Snapshot = { key: string; total: number; version: number };
type ReadService = { methods: { read: (environment: unknown, context: Context) => Snapshot } };
type AccountsService = {
  methods: ReadService["methods"] & {
    add: (environment: unknown, context: Context, amount: number) => number;
  };
};
type ConsumerService = {
  methods: ReadService["methods"] & {
    write: (environment: unknown, context: Context) => number;
    scopedRead: (environment: unknown, context: Context) => Promise<Snapshot>;
  };
};

test("independent orb bundles share typed identities while preserving authority and reload", async () => {
  const root = await mkdtemp(path.join(tmpdir(), "tembra-typed-bundles-"));
  const host = createHost({ principal: "admin" });
  const sdk = JSON.stringify(fileURLToPath(new URL("../src/index.ts", import.meta.url)));
  const providerSource = (version: number) => `
    import { orb, TembraError } from ${sdk};
    export { TembraError as BundledError };
    export const Accounts = Object.freeze({
      create(self) { return { key: self.key.join("/"), state: self.state({ total: 0 }) }; },
      access(principal) { return { methods: principal === "guest" ? ["read"] : ["read", "add"] }; },
      methods: {
        read({ key, state }) { return { key, total: state.value.total, version: ${version} }; },
        add({ state }, ctx, amount) {
          return state.update(ctx, value => ({ total: value.total + amount })).total;
        }
      }
    });
    export const Ledger = {
      create(self) { return { account: self.get(Accounts, self.key) }; },
      methods: { read({ account }, ctx) { return account.read(ctx); } }
    };
    export default orb({ name: "bundle-provider", apiVersion: 1, services: { accounts: Accounts, ledger: Ledger } });
  `;
  try {
    await writeFile(path.join(root, "provider.ts"), providerSource(1));
    await writeFile(
      path.join(root, "consumer.ts"),
      `
      import { orb, TembraError } from ${sdk};
      import { Accounts } from "./provider.ts";
      export { Accounts as ImportedAccounts, TembraError as BundledError };
      export let scoped;
      export const Consumer = {
        create(self) { return { account: self.get(Accounts, self.key) }; },
        methods: {
          read({ account }, ctx) { return account.read(ctx); },
          write({ account }, ctx) { return account.add(1, ctx); },
          async scopedRead(_environment, ctx) {
            scoped = await ctx.get(Accounts, "scoped");
            return scoped.read(ctx);
          }
        }
      };
      export const Undeclared = orb({
        name: "bundle-undeclared", apiVersion: 1,
        services: { undeclared: { create(self) { return self.get(Accounts, "denied"); }, methods: {} } }
      });
      export default orb({
        name: "bundle-consumer", apiVersion: 1,
        requires: { "bundle-provider": { apiVersion: 1, services: { accounts: ["read"] } } },
        services: { consumer: Consumer }
      });
    `,
    );
    const artifacts = await build({
      root,
      builds: {
        provider: { entry: "provider.ts", platform: "node", typecheck: false },
        consumer: { entry: "consumer.ts", platform: "node", typecheck: false },
      },
    });
    const provider: {
      Accounts: AccountsService;
      Ledger: ReadService;
      BundledError: unknown;
      default: OrbDefinition & { services: { accounts: AccountsService } };
    } = await import(pathToFileURL(artifacts[0]!.path).href);
    const consumer: {
      ImportedAccounts: AccountsService;
      Consumer: ConsumerService;
      BundledError: unknown;
      Undeclared: OrbDefinition;
      scoped: ReferenceBase;
    } = await import(pathToFileURL(artifacts[1]!.path).href);
    expect(provider.BundledError).not.toBe(TembraError);
    expect(consumer.BundledError).not.toBe(provider.BundledError);
    expect(consumer.ImportedAccounts).not.toBe(provider.Accounts);
    expect(Object.isFrozen(provider.Accounts)).toBe(true);
    await host.load(artifacts[0]!.path);
    await host.load(artifacts[1]!.path);
    await host.install(consumer.Undeclared);

    const original = await host.get(provider.Accounts, "retained");
    const captured = await host.get(provider.default.services.accounts, "retained");
    const imported = await host.get(consumer.ImportedAccounts, "retained");
    const ledger = await host.get(provider.Ledger, "retained");
    const client = await host.get(consumer.Consumer, "retained");
    expect(await original.add(5)).toBe(5);
    for (const reference of [captured, imported, ledger, client])
      expect(await reference.read()).toEqual({ key: "retained", total: 5, version: 1 });
    expect(await client.scopedRead()).toEqual({ key: "scoped", total: 0, version: 1 });
    expect(consumer.scoped.status).toBe(Status.Released);
    await expect(client.write()).rejects.toBeInstanceOf(TypeError);
    await expect(host.get("undeclared", "main")).rejects.toMatchObject({
      code: "forbidden",
      reason: "undeclared_dependency",
    });
    const guest = await host.get(provider.Accounts, "guest", { principal: "guest" });
    expect(guest.add).toBeUndefined();
    expect(() => guest.add(1)).toThrow(TypeError);
    guest.release();

    await writeFile(path.join(root, "provider.ts"), providerSource(2));
    const [replacement] = await build({
      root,
      builds: { provider: { entry: "provider.ts", platform: "node", typecheck: false } },
    });
    const fresh = await import(pathToFileURL(replacement!.path).href);
    await host.reload([fresh.default]);
    for (const reference of [original, captured, imported, ledger, client]) {
      expect(await reference.read()).toEqual({ key: "retained", total: 5, version: 2 });
      reference.release();
    }
    expect(host.errors()).toEqual([]);
  } finally {
    await host.close();
    await rm(root, { recursive: true, force: true });
  }
  expect(host.diagnostics()).toMatchObject({
    objects: 0,
    references: 0,
    calls: 0,
    values: 0,
    pendingJobs: 0,
  });
});
