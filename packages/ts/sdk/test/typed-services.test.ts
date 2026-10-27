import { expect, test } from "bun:test";
import {
  orb,
  Status,
  type Context,
  type Declaration,
  type ServiceDefinition,
} from "../src/index.ts";
import { createHost } from "../src/node.ts";

function accountService() {
  const create = (self: Declaration) => ({
    state: self.state({ key: self.key.join("/"), balance: 0 }),
  });
  return {
    create,
    methods: {
      read: ({ state }: ReturnType<typeof create>) => state.value,
      add({ state }: ReturnType<typeof create>, ctx: Context, amount: number) {
        return state.update(ctx, (value) => ({ ...value, balance: value.balance + amount }))
          .balance;
      },
    },
  } satisfies ServiceDefinition;
}

export const Accounts = accountService();

test("typed factory dependencies refuse aliases for a different service name", async () => {
  const Token = { methods: { read: () => "token" } } satisfies ServiceDefinition;
  orb({ name: "typed-alias-token", apiVersion: 1, services: { accounts: Token } });
  const host = createHost();
  try {
    await host.install([
      orb({
        name: "typed-alias-provider",
        apiVersion: 1,
        services: { store: { methods: { read: () => "store" } } },
      }),
      orb({
        name: "typed-alias-consumer",
        apiVersion: 1,
        requires: {
          accounts: { orb: "typed-alias-provider", apiVersion: 1, methods: ["read"] },
        },
        services: {
          dynamic: {
            create: (self) => self.get("accounts", "main"),
            methods: { read: (ref: { read(): Promise<string> }) => ref.read() },
          },
          typed: { create: (self) => self.get(Token, "main"), methods: {} },
        },
      }),
    ]);
    const dynamic = await host.get("dynamic", "main");
    expect(await dynamic.read()).toBe("store");
    await expect(host.get("typed", "main")).rejects.toMatchObject({
      code: "invalid",
      reason: "service",
    });
    dynamic.release();
  } finally {
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, values: 0, calls: 0 });
});

test("plain service objects and their captured copies identify distinct services and keys", async () => {
  const Labels = {
    create: (self: Declaration) => self.key.join("/"),
    methods: { label: (key: string) => `label:${key}` },
  } satisfies ServiceDefinition;
  const definition = orb({
    name: "typed-identity",
    apiVersion: 1,
    services: { accounts: Accounts, labels: Labels },
  });
  expect(Object.isFrozen(definition.services.accounts)).toBe(true);
  expect(definition.services.accounts).not.toBe(Accounts);
  const host = createHost();
  try {
    await host.install(definition);
    const account = await host.get(Accounts, ["team", "first"]);
    const captured = await host.get(definition.services.accounts, ["team", "first"]);
    const other = await host.get(Accounts, "second");
    const label = await host.get(Labels, "first");
    expect(await account.add(7)).toBe(7);
    expect(await captured.read()).toEqual({ key: "team/first", balance: 7 });
    expect(await other.read()).toEqual({ key: "second", balance: 0 });
    expect(await label.label()).toBe("label:first");
    const dynamic = await host.get("accounts", ["team", "first"]);
    expect(await dynamic.read()).toEqual({ key: "team/first", balance: 7 });
    for (const reference of [account, captured, other, label, dynamic]) reference.release();
  } finally {
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, values: 0, calls: 0 });
});

test("the same service identity can be recaptured and installed in separate hosts", async () => {
  const Service = accountService();
  const definition = orb({ name: "typed-recapture", apiVersion: 1, services: { item: Service } });
  const capturedAgain = orb(definition);
  const originalAgain = orb({
    name: "typed-recapture",
    apiVersion: 1,
    services: { item: Service },
  });
  const first = createHost();
  const second = createHost();
  try {
    await first.install(capturedAgain);
    await second.install(originalAgain);
    const one = await first.get(Service, "main");
    const two = await second.get(definition.services.item, "main");
    expect(await one.add(3)).toBe(3);
    expect(await two.read()).toEqual({ key: "main", balance: 0 });
    one.release();
    two.release();
  } finally {
    await Promise.all([first.close(), second.close()]);
  }
});

test("conflicting service identity declarations are refused", () => {
  for (const conflict of ["orb", "name", "version"] as const) {
    const Service = accountService();
    orb({ name: "typed-conflict", apiVersion: 1, services: { item: Service } });
    expect(() =>
      orb({
        name: conflict === "orb" ? "other-orb" : "typed-conflict",
        apiVersion: conflict === "version" ? 2 : 1,
        services: { [conflict === "name" ? "other-service" : "item"]: Service },
      }),
    ).toThrow(expect.objectContaining({ code: "invalid", reason: "service" }));
  }
  const Aliased = accountService();
  expect(() =>
    orb({ name: "typed-alias", apiVersion: 1, services: { first: Aliased, second: Aliased } }),
  ).toThrow(expect.objectContaining({ code: "invalid", reason: "service" }));
});

test("a failed declaration does not reserve any service identity", () => {
  for (const failure of ["service", "hook", "conflict"] as const) {
    const Fresh = accountService();
    const Taken = accountService();
    orb({ name: "typed-taken", apiVersion: 1, services: { taken: Taken } });
    expect(() =>
      orb({
        name: "typed-failed",
        apiVersion: 1,
        services: {
          fresh: Fresh,
          ...(failure === "service"
            ? { invalid: { methods: { broken: null } } as unknown as ServiceDefinition }
            : failure === "conflict"
              ? { taken: Taken }
              : {}),
        },
        ...(failure === "hook" ? { hooks: { broken: {} as { run: () => void } } } : {}),
      }),
    ).toThrow();
    expect(() =>
      orb({ name: "typed-recovered", apiVersion: 1, services: { recovered: Fresh } }),
    ).not.toThrow();
  }
});

test("unregistered objects fail before acquiring a runtime reference", async () => {
  const host = createHost();
  const Unknown = accountService();
  try {
    const before = host.diagnostics();
    expect(() => host.hold(Unknown, "main")).toThrow(
      expect.objectContaining({ code: "invalid", reason: "service" }),
    );
    await expect(host.get(Unknown, "main")).rejects.toMatchObject({
      code: "invalid",
      reason: "service",
    });
    expect(host.diagnostics()).toEqual(before);
  } finally {
    await host.close();
  }
});

test("typed hold activates normally and root acquisitions expire with their root", async () => {
  const Service = accountService();
  const host = createHost();
  try {
    await host.install(orb({ name: "typed-lifetime", apiVersion: 1, services: { item: Service } }));
    const held = host.hold(Service, "held");
    await held.ready;
    expect(await held.add(2)).toBe(2);
    const escaped = await host.root(async (ctx) => {
      const reference = await ctx.get(Service, "root");
      expect(await reference.add(4, ctx)).toBe(4);
      return reference;
    });
    expect(escaped.status).toBe(Status.Released);
    expect(() => escaped.read()).toThrow(
      expect.objectContaining({ code: "expired", reason: "root" }),
    );
    expect(await held.read()).toEqual({ key: "held", balance: 2 });
    held.release();
  } finally {
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, values: 0, calls: 0 });
});

test("typed dependencies retain declared requirements and method restrictions", async () => {
  const Provider = accountService();
  const host = createHost();
  const provider = orb({ name: "typed-provider", apiVersion: 1, services: { accounts: Provider } });
  const Consumer = {
    create(self: Declaration) {
      return { account: self.get(Provider, "dependency", { methods: ["read", "add"] }) };
    },
    methods: {
      async read({ account }: { account: ReturnType<Declaration["get"]> }, ctx: Context) {
        expect(account.add).toBeUndefined();
        return account.read(ctx);
      },
    },
  } satisfies ServiceDefinition;
  try {
    await host.install([
      provider,
      orb({
        name: "typed-consumer",
        apiVersion: 1,
        requires: { "typed-provider": { apiVersion: 1, services: { accounts: ["read"] } } },
        services: { consumer: Consumer },
      }),
      orb({
        name: "typed-denied",
        apiVersion: 1,
        services: {
          denied: { create: (self) => self.get(Provider, "forbidden"), methods: {} },
        },
      }),
    ]);
    const consumer = await host.get(Consumer, "main");
    expect(await consumer.read()).toEqual({ key: "dependency", balance: 0 });
    await expect(host.get("denied", "main")).rejects.toMatchObject({
      code: "forbidden",
      reason: "undeclared_dependency",
    });
    consumer.release();
  } finally {
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, values: 0, calls: 0 });
});

test("typed acquisition preserves principal access and explicit method views", async () => {
  const Service = {
    access: (principal: string) => ({
      methods: principal === "guest" ? ["read"] : ["read", "write"],
    }),
    methods: { read: () => "read", write: () => "write" },
  } satisfies ServiceDefinition;
  const host = createHost();
  try {
    await host.install(orb({ name: "typed-access", apiVersion: 1, services: { item: Service } }));
    const guest = await host.get(Service, "main", { principal: "guest" });
    const restricted = await host.get(Service, "main", { principal: "admin", methods: ["read"] });
    const admin = await host.get(Service, "main", { principal: "admin" });
    expect(guest.write).toBeUndefined();
    expect(restricted.write).toBeUndefined();
    expect(await guest.read()).toBe("read");
    expect(await admin.write()).toBe("write");
    guest.release();
    restricted.release();
    admin.release();
  } finally {
    await host.close();
  }
});

test("typed identity resolves current code after reload and rejects missing or incompatible providers", async () => {
  const First = { methods: { read: () => 1 } } satisfies ServiceDefinition;
  const Second = { methods: { read: () => 2 } } satisfies ServiceDefinition;
  const host = createHost();
  const initial = orb({ name: "typed-reload", apiVersion: 1, services: { item: First } });
  try {
    await expect(host.get(First, "missing")).rejects.toMatchObject({
      code: "unavailable",
      reason: "unknown_service",
    });
    await host.install(initial);
    const retained = await host.get(First, "main");
    await host.reload([orb({ name: "typed-reload", apiVersion: 1, services: { item: Second } })]);
    const fresh = await host.get(First, "fresh");
    expect(await retained.read()).toBe(2);
    expect(await fresh.read()).toBe(2);
    fresh.release();
    retained.release();
    await host.remove(["typed-reload"]);
    await expect(host.get(First, "removed")).rejects.toMatchObject({
      code: "unavailable",
      reason: "unknown_service",
    });
    await host.install(
      orb({
        name: "typed-reload",
        apiVersion: 2,
        services: { item: { methods: { read: () => 3 } } },
      }),
    );
    await expect(host.get(First, "incompatible")).rejects.toMatchObject({ reason: "api_version" });
    await expect(host.get(First, "override", { apiVersion: 2 })).rejects.toMatchObject({
      code: "invalid",
      reason: "service",
    });
  } finally {
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, values: 0, calls: 0 });
});

test("typed factory dependencies cannot change a requirement version through a cached reference", async () => {
  const Token = { methods: { read: () => 1 } } satisfies ServiceDefinition;
  orb({ name: "typed-required-provider", apiVersion: 2, services: { item: Token } });
  const host = createHost();
  try {
    await host.install([
      orb({
        name: "typed-required-provider",
        apiVersion: 1,
        services: { item: { methods: { read: () => 1 } } },
      }),
      orb({
        name: "typed-required-consumer",
        apiVersion: 1,
        requires: { "typed-required-provider": { apiVersion: 1, services: { item: ["read"] } } },
        services: {
          consumer: {
            create(self) {
              self.get("item", "main");
              return self.get(Token, "main");
            },
            methods: {},
          },
        },
      }),
    ]);
    await expect(host.get("consumer", "main")).rejects.toMatchObject({
      code: "invalid",
      reason: "service",
    });
  } finally {
    await host.close();
  }
  expect(host.diagnostics()).toMatchObject({ objects: 0, values: 0, calls: 0 });
});
