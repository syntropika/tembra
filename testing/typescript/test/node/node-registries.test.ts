import assert from "node:assert/strict";
import test from "node:test";

import { Type, defineFacet, defineRegistry, local, method } from "@tembra/sdk";
import type { Registry, RegistrySnapshot } from "@tembra/sdk";
import { createHost } from "@tembra/sdk/node";
import { defined } from "@tembra/testing";

type Metadata = { title: string; nested?: { enabled: boolean } };

function service(name: string) {
  return local(name, {
    version: 1,
    input: Type.Object({}),
    members: { read: method(undefined, Type.Integer()) },
  });
}

function contribution(
  name: string,
  contract: ReturnType<typeof service>,
  registry: Registry<Metadata>,
  title: string,
  result: number,
  closed: string[],
) {
  return defineFacet({
    name,
    uses: [],
    setup(_scope, declaration) {
      declaration.serve(
        contract,
        (_input, self) => {
          self.register(registry, "metadata", { title, nested: { enabled: true } });
          self.registerMethod(registry, "read", { title }, "read");
          const resource = self.resource("owned", {
            version: result,
            open: () => ({ result }),
            close: () => closed.push(title),
          });
          return { state: {}, methods: { read: () => resource.value.result } };
        },
        { lifetime: "host" },
      );
    },
  });
}

await test("typed registry definitions preserve immutable snapshots, operations and reload ownership", async () => {
  const host = await createHost();
  const contract = service("test.registry.typed.reload");
  const commands = defineRegistry<Metadata>("test.registry.typed.commands");
  const closed: string[] = [];
  const notifications: RegistrySnapshot<Metadata>[] = [];
  let remove;
  let reference;
  try {
    await host.install(contribution("commands", contract, commands, "First", 1, closed));
    reference = await host.get(contract, {});
    const view = host.registry(commands);
    remove = view.subscribe((snapshot) => {
      notifications.push(snapshot);
    });
    const before = view.table();
    const original = defined(defined(before.get("read")).operation);
    await host.root(async (context) => {
      assert.deepEqual(await original.call(context, { present: false }), {
        present: true,
        value: 1,
      });
    });
    assert.equal(Object.isFrozen(before.entries), true);
    assert.equal(Object.isFrozen(defined(defined(before.get("metadata")).data.nested)), true);
    assert.throws(() => {
      defined(defined(before.get("metadata")).data.nested).enabled = false;
    }, TypeError);

    const report = await host.reload([
      contribution("commands", contract, commands, "Second", 2, closed),
    ]);
    const after = view.table();
    assert.ok(after.codeVersion > before.codeVersion);
    assert.equal(report.codeVersion, after.codeVersion);
    assert.equal(defined(before.get("metadata")).data.title, "First");
    assert.equal(defined(after.get("metadata")).data.title, "Second");
    assert.equal(await reference.read(), 2);
    await host.root(async (context) => {
      await assert.rejects(original.call(context, { present: false }));
      assert.deepEqual(
        await defined(defined(after.get("read")).operation).call(context, { present: false }),
        {
          present: true,
          value: 2,
        },
      );
    });
    assert.equal(defined(defined(notifications.at(-1)).get("read")).data.title, "Second");
    remove();
    remove = undefined;
    reference.release();
    reference = undefined;
    await host.close();
    assert.equal(defined(before.get("metadata")).data.title, "First");
    assert.equal(defined(after.get("metadata")).data.title, "Second");
    assert.ok(closed.includes("First"));
    assert.ok(closed.includes("Second"));
    assert.equal(host.diagnostics().bridge.values, 0);
  } finally {
    remove?.();
    reference?.release();
    await host.close();
  }
});

await test("registry conflict policy and portable contribution validation remain Rust-backed", async () => {
  for (const conflict of ["error", "last"] as const) {
    // oxlint-disable-next-line no-await-in-loop -- Each policy owns an independent real Host.
    const host = await createHost();
    const registry = defineRegistry<Metadata>(`test.registry.policy.${conflict}`, { conflict });
    const closed: string[] = [];
    try {
      // oxlint-disable-next-line no-await-in-loop -- Establish the first contribution before admitting the conflicting one.
      await host.install(
        contribution(
          "first",
          service(`test.registry.${conflict}.first`),
          registry,
          "First",
          1,
          closed,
        ),
      );
      const candidate = contribution(
        "second",
        service(`test.registry.${conflict}.second`),
        registry,
        "Second",
        2,
        closed,
      );
      if (conflict === "error") {
        // oxlint-disable-next-line no-await-in-loop -- Verify refusal before observing the surviving table.
        await assert.rejects(host.install(candidate), { reason: "registry_conflict" });
        assert.equal(defined(host.registry(registry).table().get("read")).data.title, "First");
      } else {
        // oxlint-disable-next-line no-await-in-loop -- The selected value is observable only after activation completes.
        await host.install(candidate);
        assert.equal(defined(host.registry(registry).table().get("read")).data.title, "Second");
      }
      assert.throws(
        () =>
          host.registry(
            defineRegistry(registry.name, {
              conflict: conflict === "error" ? "last" : "error",
            }),
          ),
        { reason: "registry_definition" },
      );
    } finally {
      // oxlint-disable-next-line no-await-in-loop -- Join physical cleanup before starting the next Host.
      await host.close();
    }
  }

  const host = await createHost();
  const registry = defineRegistry("test.registry.invalid.data");
  try {
    const invalid = defineFacet({
      name: "invalid-metadata",
      uses: [],
      setup(_scope, declaration) {
        declaration.serve(
          service("test.registry.invalid.service"),
          (_input, self) => {
            // @ts-expect-error: Deliberately exercise the runtime rejection for JavaScript callers.
            self.register(registry, "invalid", { title: "Unsafe", execute: () => 0 });
            return { state: {}, methods: { read: () => 0 } };
          },
          { lifetime: "host" },
        );
      },
    });
    await assert.rejects(host.install(invalid));
    assert.equal(host.registry(registry).table().entries.length, 0);
  } finally {
    await host.close();
  }
});
