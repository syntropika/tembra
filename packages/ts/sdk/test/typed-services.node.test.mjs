import { test } from "node:test";
import assert from "node:assert/strict";
import { orb, Status } from "../dist/index.js";
import { createHost } from "../dist/node.js";

test("published entrypoints share plain service identity and root lifetimes", async () => {
  const Accounts = {
    create(self) {
      return { key: self.key.join("/") };
    },
    methods: { read: ({ key }) => key },
  };
  const definition = orb({ name: "typed-node", apiVersion: 1, services: { accounts: Accounts } });
  const host = createHost();
  try {
    await host.install(definition);
    const original = await host.get(Accounts, ["team", "account"]);
    const captured = await host.get(definition.services.accounts, "captured");
    assert.equal(await original.read(), "team/account");
    assert.equal(await captured.read(), "captured");
    const scoped = await host.root(async (ctx) => {
      const reference = await ctx.get(Accounts, "root");
      assert.equal(await reference.read(ctx), "root");
      return reference;
    });
    assert.equal(scoped.status, Status.Released);
    assert.throws(() => scoped.read(), { code: "expired", reason: "root" });
    original.release();
    captured.release();
  } finally {
    await host.close();
  }
  assert.equal(host.diagnostics().objects, 0);
  assert.equal(host.diagnostics().values, 0);
});
