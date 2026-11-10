// `Context.get` from an isolated source Orb, through the real kernel, SDK roots, `Revision` and
// `Scope`. Only the guest port is a double: it issues the same host operations the interpreter
// runtime sends for `await context.get(binding, key)`. The provider is an ordinary JavaScript Orb
// so each instance reports the kernel address (key and principal) it was activated for.
import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { NodeContract } from "@syntropika/tembra-engine-node";
import { isolate } from "#tembra-adapters";
import { createHost } from "../src/node.ts";
import { orb } from "../src/orb.ts";
import { deferred } from "../src/runtime/records.ts";
import { reply } from "../src/interpreter/codec.ts";
import { Revision } from "../src/interpreter/revision.ts";
import type { Context } from "../src/runtime/context.ts";
import type { ContractPort, ExecutionScope, InterpreterPort } from "../src/interpreter/types.ts";

const COUNTER = "example:keyed/counter";
const PROVIDED = `provider::${COUNTER}`;
const WIT = `package example:keyed;
interface counter {
  add: func(amount: u32) -> u32;
  who: func() -> string;
}
interface router { route: func(key: string, amount: u32) -> u32; }
world consumer { import counter; export router; }`;

const directory = await mkdtemp(path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "wit-"));
await writeFile(path.join(directory, "contract.wit"), WIT);
const graph = JSON.parse(new NodeContract(directory, "consumer").request('{"kind":"describe"}')).ok;
await rm(directory, { recursive: true, force: true });

/**
 * Keyed counters; the `slow` key stays unready until `opening` resolves. `activated` records the
 * address of every instance the provider created.
 */
function provider(opening: Promise<void>, activated: string[]) {
  return orb({
    name: "provider",
    apiVersion: 1,
    services: {
      [PROVIDED]: {
        create(self) {
          const instance = {
            key: self.key.join("/"),
            principal: self.principal,
            count: 0,
          };
          activated.push(`${instance.key}@${instance.principal}`);
          if (instance.key === "slow")
            self.resource("connection", { open: () => opening.then(() => 1) });
          return instance;
        },
        methods: {
          add: (instance: { count: number }, _context: unknown, amount: number) =>
            (instance.count += amount),
          who: (instance: { key: string; principal: string }) =>
            `${instance.key}@${instance.principal}`,
        },
      },
    },
  });
}

type Route = (scope: ExecutionScope, key: string, amount: number) => Promise<number>;
/** Guest callback numbers of the router service's `session` resource. */
const OPEN = 1;
const CLOSE = 2;

/**
 * The consumer's guest double runs the current `route` for each call and records what its
 * service factory could do with a declaration authority. `methods` is the installed requirement.
 */
function consumer(methods: string[]) {
  const harness = {
    route: (async () => 0) as Route,
    declared: [] as string[],
    /** Outcomes of `Context.get` attempted by the `session` resource's close callback. */
    closes: [] as string[],
    closed: deferred<void>(),
    /** The admitted context of the latest service call, captured at the host `Scope` seam. */
    context: undefined as Context | undefined,
    revision: undefined as Revision | undefined,
    /** The service environment's host table, where `Context.get` handles are issued. */
    get table() {
      return [...harness.revision!.scopes.values()].find((scope) => scope.service !== null)!;
    },
  };
  const port: InterpreterPort = {
    description: () => "{}",
    async declare(packet, scope) {
      if (JSON.parse(packet).service === null) return null;
      // A factory has no invocation, so it cannot select instances at runtime.
      await scope
        .async({ operation: "reference.acquire", service: COUNTER, key: ["a"] })
        .catch((error: Error) => harness.declared.push(error.message));
      // A service-declared resource whose close tries to select an instance, like
      // `resource("session", { close: (value, context) => context.get(...) })`.
      scope.sync({
        operation: "resource.declare",
        name: "session",
        version: 1,
        open: OPEN,
        close: CLOSE,
        dependencies: [],
      });
      return null;
    },
    async execute(packet, scope) {
      const request = JSON.parse(packet);
      if (request.kind === "callback" && request.callback === OPEN) return "1";
      if (request.kind === "callback" && request.callback === CLOSE) {
        harness.closes.push(await refusal(acquire(scope, "closing")));
        harness.closed.resolve();
        return "null";
      }
      return JSON.stringify(await harness.route(scope, ...(request.args as [string, number])));
    },
    release() {},
    close() {},
  };
  let slot = 0;
  const contract: ContractPort = {
    bind: () => ++slot,
    validate: () => '{"ok":null}',
    request: (packet) =>
      JSON.stringify({
        ok:
          JSON.parse(packet).kind === "describe"
            ? graph
            : {
                services: [
                  {
                    name: `consumer::example:keyed/router`,
                    interface: "example:keyed/router",
                    alias: "router",
                    methods: { route: "route" },
                    lifetime: "referenced",
                    linger: 0,
                    signature: "sig-router",
                  },
                ],
                hooks: { hooks: [], contributions: [] },
                plan,
              },
      }),
  };
  const plan = {
    orb: {
      name: "consumer",
      apiVersion: 1,
      services: [],
      requires: [
        {
          orb: "provider",
          apiVersion: 1,
          services: { [PROVIDED]: methods },
          signatures: {},
          hooks: [],
          hookSignatures: {},
        },
      ],
      hooks: [],
      hookSignatures: {},
    },
  };
  const isolated = async () => {
    harness.revision = new Revision(port, contract, 5000);
    return harness.revision.definition(
      { name: "consumer", apiVersion: 1 },
      {
        bindings: {
          [COUNTER]: { kind: "orb" as const, name: "provider", api_version: 1 },
        },
        importServices: { [COUNTER]: PROVIDED },
        plan,
      },
    );
  };
  return { harness, isolated };
}

/** Install the provider and the consumer; return the host and a router reference. */
async function served(methods = ["add", "who"], opening: Promise<void> = Promise.resolve()) {
  const { harness, isolated } = consumer(methods);
  const activated: string[] = [];
  const host = createHost({ principal: "test", ...{ [isolate]: isolated } });
  await host.install(provider(opening, activated));
  await host.install({ artifact: Uint8Array.of(0) } as never);
  const router = (await host.get("consumer::example:keyed/router", "main")) as any;
  // Internal seam: record the admitted context the host hands to each service callback.
  const table = harness.table;
  const exchange = table.exchange.bind(table);
  table.exchange = (authority) => {
    if ("context" in authority) harness.context = authority.context;
    return exchange(authority);
  };
  return { host, harness, router, activated };
}

const acquire = (scope: ExecutionScope, key: string) =>
  scope.async({
    operation: "reference.acquire",
    service: COUNTER,
    key: [key],
  }) as Promise<number>;
/** Call an acquired import and decode its checked wire reply. */
const call = (scope: ExecutionScope, id: number, method: string, args: unknown[] = []) =>
  scope
    .async({ operation: "reference.call", id, method, args })
    .then((value) => JSON.parse(reply(value)));
const refusal = (operation: Promise<unknown>) =>
  operation.then(
    () => "accepted",
    (error: Error) => error.message,
  );

async function settle(host: ReturnType<typeof createHost>) {
  await host.close();
  const diagnostics = host.diagnostics();
  for (const name of ["objects", "references", "roots", "calls", "values", "pendingJobs"] as const)
    expect(diagnostics[name]).toBe(0);
}

test("keys select instances, one key is shared within a root, principals stay isolated", async () => {
  const { host, harness, router } = await served();
  try {
    expect(harness.declared).toEqual(["Operation requires an admitted invocation"]);
    const seen: unknown[] = [];
    harness.route = async (scope, key, amount) => {
      const first = await acquire(scope, key);
      const again = await acquire(scope, key);
      const other = await acquire(scope, `${key}-other`);
      expect(new Set([first, again, other]).size).toBe(3);
      seen.push(
        await call(scope, first, "add", [amount]),
        await call(scope, again, "add", [amount]),
        await call(scope, other, "add", [amount]),
        await call(scope, again, "who"),
        await call(scope, other, "who"),
      );
      return 0;
    };
    for (const principal of ["alice", "bob"])
      await host.root((context) => router.route("a", 2, context), {
        principal,
      });
    expect(seen).toEqual([
      ...[2, 4, 2, "a@alice", "a-other@alice"],
      ...[2, 4, 2, "a@bob", "a-other@bob"],
    ]);
    // Every root claim ended with its root: only the application's router reference remains.
    expect(host.diagnostics().references).toBe(1);
  } finally {
    router.release();
    await settle(host);
  }
});

test("a Context.get handle serves only its own live root and is forgotten afterwards", async () => {
  const { host, harness, router } = await served();
  try {
    let retained = 0;
    harness.route = async (scope) => {
      retained = await acquire(scope, "a");
      // Explicit release is idempotent while the root lasts; the handle then reports it.
      scope.sync({ operation: "reference.release", id: retained });
      scope.sync({ operation: "reference.release", id: retained });
      expect(scope.sync({ operation: "reference.status", id: retained })).toBe("released");
      // A released key may still be closing its only instance, so the root acquires another.
      retained = await acquire(scope, "c");
      return (await call(scope, retained, "add", [1])) as number;
    };
    await host.root((context) => router.route("a", 0, context), {
      principal: "alice",
    });
    expect(harness.table.references.size).toBe(2);
    const refusals: string[] = [];
    harness.route = async (scope) => {
      // Same principal, later root: the retained number names nothing usable.
      refusals.push(
        await refusal(call(scope, retained, "add", [1])),
        await refusal(
          Promise.resolve().then(() => scope.sync({ operation: "reference.status", id: retained })),
        ),
        await refusal(
          Promise.resolve().then(() =>
            scope.sync({ operation: "reference.release", id: retained }),
          ),
        ),
        await refusal(scope.async({ operation: "reference.ready", id: retained })),
        await refusal(call(scope, retained, "add", [1]).then(() => 0)),
        // A foreign table selector is refused before any handle lookup.
        await refusal(
          scope.async({
            operation: "reference.ready",
            scope: 999,
            id: retained,
          }),
        ),
        // Only selected WIT Orb imports can be acquired.
        await refusal(
          scope.async({
            operation: "reference.acquire",
            service: PROVIDED,
            key: ["a"],
          }),
        ),
        await refusal(
          scope.async({
            operation: "reference.acquire",
            service: "other",
            key: ["a"],
          }),
        ),
      );
      // The next acquisition forgets the ended root's names; the new one remains.
      await acquire(scope, "b");
      expect(harness.table.references.size).toBe(1);
      return 0;
    };
    await host.root((context) => router.route("a", 0, context), {
      principal: "alice",
    });
    expect(refusals).toEqual([
      ...Array(5).fill("Reference belongs to another root"),
      "Foreign environment handle",
      "Reference is not a selected WIT Orb import",
      "Reference is not a selected WIT Orb import",
    ]);
    expect(host.diagnostics().references).toBe(1);
  } finally {
    router.release();
    await settle(host);
  }
});

test("the installed requirement, not the guest, decides the methods of an acquired reference", async () => {
  const { host, harness, router } = await served(["add"]);
  try {
    const outcomes: string[] = [];
    harness.route = async (scope) => {
      const id = await acquire(scope, "a");
      outcomes.push(
        String(await call(scope, id, "add", [3])),
        await refusal(call(scope, id, "who")),
      );
      return 0;
    };
    await host.root((context) => router.route("a", 0, context), {
      principal: "alice",
    });
    expect(outcomes).toEqual(["3", "Reference does not expose this method"]);
  } finally {
    router.release();
    await settle(host);
  }
});

test("cancellation during readiness releases the claim at once and publishes no handle", async () => {
  const opening = deferred<void>();
  const { host, harness, router } = await served(["add", "who"], opening.promise);
  try {
    const waiting = deferred<void>();
    harness.route = async (scope) => {
      const acquisition = acquire(scope, "slow");
      waiting.resolve();
      return acquisition;
    };
    const during: number[] = [];
    const outcome = await host.root(
      async (context) => {
        const routed = router.route("slow", 0, context);
        await waiting.promise;
        context.cancel();
        const result = await refusal(routed);
        // The claim is gone before the root ends; only the application's router remains.
        during.push(host.diagnostics().references);
        return result;
      },
      { principal: "alice" },
    );
    expect(outcome).not.toBe("accepted");
    expect(during).toEqual([1]);
    expect(harness.table.references.size).toBe(0);
    opening.resolve();
  } finally {
    router.release();
    await settle(host);
  }
});

test("a resource close cannot select instances under the cleanup principal", async () => {
  const { host, harness, router, activated } = await served();
  try {
    await host.root((context) => router.route("a", 1, context), { principal: "alice" });
    // Releasing the only claim disposes the router object while the host stays open, so its
    // `session` close runs now, under a cleanup root without an admitted invocation.
    router.release();
    await harness.closed.promise;
    expect(harness.closes).toEqual(["Reference acquisition requires an admitted invocation"]);
    expect(activated.filter((address) => address.startsWith("closing@"))).toEqual([]);
    expect(host.diagnostics().references).toBe(0);
  } finally {
    await settle(host);
  }
});

test("a root handle issued into the Orb table is refused from a service table in the same root", async () => {
  const { host, harness, router } = await served();
  try {
    const outcomes: string[] = [];
    harness.route = async (scope) => {
      // Internal seam with the real kernel context: acquire in the Orb (parent) table under the
      // same admitted context, as a contribution in this root would.
      const parent = harness.table.parent!;
      const orbScope = parent.exchange({ context: harness.context! });
      const id = await acquire(orbScope, "a");
      outcomes.push(
        String(await call(orbScope, id, "add", [1])),
        await refusal(call(scope, id, "add", [1]).then(() => 0)),
        await refusal(
          scope.async({
            operation: "reference.call",
            scope: parent.id,
            id,
            method: "add",
            args: [1],
          }),
        ),
      );
      orbScope.cancel();
      return 0;
    };
    await host.root((context) => router.route("a", 0, context), { principal: "alice" });
    // The service table never issued it; the Orb table's handle is root-scoped to that table.
    expect(outcomes).toEqual([
      "1",
      "Unknown reference handle",
      "Reference belongs to another root",
    ]);
  } finally {
    router.release();
    await settle(host);
  }
});

test("a root handle expires once the root's claims are revoked, before the callback settles", async () => {
  const { host, harness, router } = await served();
  try {
    const outcomes: string[] = [];
    harness.route = async (scope) => {
      const id = await acquire(scope, "a");
      // Internal seam: the runtime revokes a root's claims when the root ends, which may happen
      // while this callback is still physically settling. Status reads skip the context check.
      harness.table.declaration.session.calls.releaseRoot(harness.context!.root);
      outcomes.push(
        await refusal(
          Promise.resolve().then(() => scope.sync({ operation: "reference.status", id })),
        ),
      );
      return 0;
    };
    await host.root((context) => router.route("a", 0, context), { principal: "alice" });
    expect(outcomes).toEqual(["Reference expired with its root"]);
  } finally {
    router.release();
    await settle(host);
  }
});

test("authority is checked again after readiness: an ended root publishes no handle", async () => {
  const opening = deferred<void>();
  const { host, harness, router } = await served(["add", "who"], opening.promise);
  try {
    // Another claim on the same instance keeps it activating after the root's claim is revoked,
    // so readiness itself succeeds and only the authority recheck can refuse publication.
    const keeper = host.get(PROVIDED, "slow", { principal: "alice" });
    const waiting = deferred<void>();
    let routed: Promise<string> | undefined;
    harness.route = async (scope) => {
      const acquisition = acquire(scope, "slow");
      waiting.resolve();
      return acquisition;
    };
    // The root ends without cancelling: its caller stops waiting while the callback continues.
    await host.root(
      async (context) => {
        routed = refusal(router.route("slow", 0, context));
        await waiting.promise;
      },
      { principal: "alice" },
    );
    opening.resolve();
    (await keeper).release();
    expect(await routed).not.toBe("accepted");
    expect(harness.table.references.size).toBe(0);
  } finally {
    router.release();
    await settle(host);
  }
});

test("a callback that ends right after readiness publishes no handle", async () => {
  const { host, harness, router } = await served();
  try {
    // A live instance: readiness settles before anything else can happen.
    const keeper = await host.get(PROVIDED, "a", { principal: "alice" });
    let acquisition: Promise<string> | undefined;
    harness.route = async (scope) => {
      acquisition = refusal(acquire(scope, "a"));
      // The callback scope ends synchronously after readiness won, before acquire resumes.
      scope.cancel();
      return 0;
    };
    await host.root((context) => router.route("a", 0, context), { principal: "alice" });
    expect(await acquisition).toBe("Interpreter callback scope ended");
    expect(harness.table.references.size).toBe(0);
    keeper.release();
  } finally {
    router.release();
    await settle(host);
  }
});
