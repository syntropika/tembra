// The kernel's interfaces (`tembra:guest`) in prepared browser components, on a real host and a
// real contract, through the components of `compiled-components.ts`.
import { describe, expect, test } from "bun:test";
import { orb } from "../src/orb.ts";
import type { Context } from "../src/runtime/context.ts";
import { compiledDescription, type Described } from "../src/interpreter/component/revision.ts";
import { query } from "../src/interpreter/types.ts";
import { contract, host, refusal, type Compiled } from "./compiled-components.ts";

const COUNTER = `package test:counter@1.0.0;
interface counter {
  add: func(by: s64) -> s64;
}
world tally {
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export counter;
}`;

describe("kernel state of compiled service objects", () => {
  test("each object declares its state; updates are validated; a reload validates and keeps it", async () => {
    const validated: string[] = [];
    const counter: Compiled = {
      name: "tally",
      contract: await contract(COUNTER, "tally"),
      component: ({ "tembra:guest/state@1.0.0": state }, revision) => ({
        "tembra:guest/state-init@1.0.0": {
          declare: (service: string) =>
            service === "test:counter/counter@1.0.0"
              ? { initial: '{"total":0}', version: 1, validated: true }
              : undefined,
          validate: (_service: string, value: string) => {
            validated.push(`${revision}: ${value}`);
            // The third revision refuses what it would inherit.
            if (revision === 3) throw refusal("too much to inherit");
          },
        },
        "test:counter/counter@1.0.0": {
          add: (by: bigint) => {
            const { total } = JSON.parse(state!.read()) as { total: number };
            const stored = state!.update(JSON.stringify({ total: total + Number(by) }));
            return BigInt((JSON.parse(stored) as { total: number }).total);
          },
        },
      }),
    };
    const tally = host([counter]);
    try {
      await tally.install({ artifact: Uint8Array.of(0) });
      const main = await tally.get("tally::test:counter/counter@1.0.0", "main");
      const other = await tally.get("tally::test:counter/counter@1.0.0", "other");
      expect(await main.add(5n)).toBe(5n);
      expect(await main.add(2n)).toBe(7n);
      expect(await other.add(1n)).toBe(1n);
      expect(main.state.value).toEqual({ total: 7 });
      // The replacement validates the state it inherits before the reload adopts it.
      await tally.reload([{ artifact: Uint8Array.of(0) }]);
      expect(validated.sort()).toEqual(['2: {"total":1}', '2: {"total":7}']);
      expect(await main.add(1n)).toBe(8n);
      // A refused validation fails the reload with the component's reason; the count stays.
      await expect(tally.reload([{ artifact: Uint8Array.of(0) }])).rejects.toThrow(
        "the state was refused: too much to inherit",
      );
      expect(await main.add(1n)).toBe(9n);
      main.release();
      other.release();
    } finally {
      await tally.close();
    }
  });

  test("a negative zero is stored as zero, so the state can still be validated on reload", async () => {
    const validated: string[] = [];
    const signed: Compiled = {
      name: "signed",
      contract: await contract(COUNTER, "tally"),
      component: ({ "tembra:guest/state@1.0.0": state }) => ({
        "tembra:guest/state-init@1.0.0": {
          declare: () => ({ initial: '{"x":0.5}', version: 1, validated: true }),
          validate: (_service: string, value: string) => void validated.push(value),
        },
        "test:counter/counter@1.0.0": {
          // serde_json writes a value that rounds to negative zero as `-0.0`.
          add: (_by: bigint) => {
            state!.update('{"x":-0.0}');
            return 0n;
          },
        },
      }),
    };
    const tally = host([signed]);
    try {
      await tally.install({ artifact: Uint8Array.of(0) });
      const main = await tally.get("signed::test:counter/counter@1.0.0", "main");
      await main.add(0n);
      expect(Object.is((main.state.value as { x: number }).x, 0)).toBe(true);
      await tally.reload([{ artifact: Uint8Array.of(0) }]);
      expect(validated).toEqual(['{"x":0}']);
      main.release();
    } finally {
      await tally.close();
    }
  });
});

const JOURNAL = `package test:journal@1.0.0;
interface journal {
  handle: func() -> u32;
}
world journaling {
  import tembra:guest/resources@1.0.0;
  export tembra:guest/resources-init@1.0.0;
  export journal;
}`;

describe("root resources of compiled Orbs", () => {
  test("the kernel opens them with the root, services read them, teardown closes them", async () => {
    const events: string[] = [];
    const journal: Compiled = {
      name: "journal",
      contract: await contract(JOURNAL, "journaling"),
      component: ({ "tembra:guest/resources@1.0.0": resources }) => ({
        "tembra:guest/resources-init@1.0.0": {
          declare: () => [{ name: "entries", version: 1, dependencies: [] }],
          open: (name: string) => {
            events.push(`open ${name}`);
            return 7;
          },
          close: (name: string, handle: number) => void events.push(`close ${name} ${handle}`),
        },
        "test:journal/journal@1.0.0": {
          handle: () => resources!.read("entries"),
        },
      }),
    };
    const owner = host([journal]);
    await owner.install({ artifact: Uint8Array.of(0) });
    const reference = await owner.get("journal::test:journal/journal@1.0.0", "main");
    expect(await reference.handle()).toBe(7);
    expect(events).toEqual(["open entries"]);
    reference.release();
    await owner.close();
    expect(events).toEqual(["open entries", "close entries 7"]);
    expect(owner.errors()).toEqual([]);
  });
});

const GATE = `package test:gate@1.0.0;
interface guard {
  apply: func(value: s32) -> s32;
}
interface gate {
  admit: func(value: s32) -> s32;
}
world gatekeeper {
  import guard;
  export guard;
  export gate;
}`;

describe("hooks declared by compiled Orbs", () => {
  test("the import runs the kernel's plan of contributions from other Orbs", async () => {
    const GUARD = "test:gate/guard@1.0.0";
    const gate: Compiled = {
      name: "gate",
      contract: await contract(GATE, "gatekeeper"),
      roles: { hooks: { [GUARD]: "waterfall" } },
      component: ({ [GUARD]: guard }) => ({
        // Contributions answer the hook: the export is the plan's typed anchor only.
        [GUARD]: {
          apply: () => {
            throw new Error("the declared hook's own export is never called");
          },
        },
        "test:gate/gate@1.0.0": {
          admit: (value: number) => guard!.apply(value),
        },
      }),
    };
    const owner = host([gate]);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const reference = await owner.get("gate::test:gate/gate@1.0.0", "main");
      expect(await reference.admit(5)).toBe(5);
      await owner.install(
        orb({
          name: "doubler",
          apiVersion: 1,
          requires: { gate: { apiVersion: 1, hooks: [GUARD] } },
          contributes: { gate: { [GUARD]: (_: Context, value: number) => value * 2 } },
        }),
      );
      expect(await reference.admit(5)).toBe(10);
      reference.release();
    } finally {
      await owner.close();
    }
  });
});

const AWARE = `package test:aware@1.0.0;
interface probe {
  whoami: func() -> string;
  where: func() -> list<string>;
  give-up: func() -> bool;
}
world aware {
  import tembra:guest/context@1.0.0;
  export probe;
}`;

describe("the kernel's context for compiled Orbs", () => {
  test("principal, address, cancellation and log of the issuing call", async () => {
    const logs: unknown[] = [];
    const aware: Compiled = {
      name: "aware",
      contract: await contract(AWARE, "aware"),
      component: ({ "tembra:guest/context@1.0.0": context }) => ({
        "test:aware/probe@1.0.0": {
          whoami: () => {
            context!.log("info", "whoami was asked");
            return context!.principal();
          },
          where: () => context!.object().key,
          giveUp: () => {
            context!.cancel();
            return context!.cancelled();
          },
        },
      }),
    };
    const owner = host([aware], logs);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const room = await owner.get("aware::test:aware/probe@1.0.0", ["room", "7"], {
        principal: "alice",
      });
      expect(await owner.root((context) => room.whoami(context), { principal: "bob" })).toBe("bob");
      expect(await room.where()).toEqual(["room", "7"]);
      expect(logs).toEqual([{ orb: "aware", level: "info", message: "whoami was asked" }]);
      // A call that cancels itself is observed cancelled by its caller.
      await expect(owner.root((context) => room["give-up"](context))).rejects.toThrow("cancelled");
      room.release();
    } finally {
      await owner.close();
    }
  });

  test("a cancelled call still reads its principal, cancels again and keeps the state", async () => {
    const POLITE = `package test:polite@1.0.0;
interface probe {
  late: func(value: string) -> string;
  read: func() -> string;
}
world polite {
  import tembra:guest/context@1.0.0;
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export probe;
}`;
    const seen: unknown[] = [];
    const polite: Compiled = {
      name: "polite",
      contract: await contract(POLITE, "polite"),
      component: ({
        "tembra:guest/context@1.0.0": context,
        "tembra:guest/state@1.0.0": state,
      }) => ({
        "tembra:guest/state-init@1.0.0": {
          declare: () => ({ initial: "0", version: 1, validated: true }),
          validate: () => {},
        },
        "test:polite/probe@1.0.0": {
          late: (value: string) => {
            context!.cancel();
            seen.push(context!.principal());
            context!.cancel();
            const retained = state!.update(value);
            seen.push(retained);
            return retained;
          },
          read: () => state!.read(),
        },
      }),
    };
    const owner = host([polite]);
    try {
      await owner.install({ artifact: Uint8Array.of(0) });
      const probe = await owner.get("polite::test:polite/probe@1.0.0", "main");
      // The guest learns its caller and that its write was not applied; its caller sees the call
      // cancelled, and nothing poisons the revision.
      await expect(
        owner.root((context) => probe.late("7", context), { principal: "alice" }),
      ).rejects.toThrow("cancelled");
      expect(seen).toEqual(["alice", "0"]);
      expect(await probe.read()).toBe("0");
      probe.release();
    } finally {
      await owner.close();
    }
  });
});

describe("compiled descriptions", () => {
  test("policies give services their lifetime; hooks and kernel callbacks are no services", async () => {
    const WIT = `package test:roles@1.0.0;
interface guard { apply: func(value: u32) -> u32; }
interface ledger { add: func(amount: u32) -> u32; }
interface journal { read: func() -> u32; }
world roles {
  import guard;
  export guard;
  import tembra:guest/state@1.0.0;
  export tembra:guest/state-init@1.0.0;
  export ledger;
  export journal;
}`;
    const roles = await contract(WIT, "roles");
    const graph = query<Described>(roles, { kind: "describe" });
    const description = compiledDescription(
      graph,
      { name: "roles", apiVersion: 1 },
      {
        contributions: {},
        hooks: { "test:roles/guard@1.0.0": "waterfall" },
        services: { "test:roles/ledger@1.0.0": { lifetime: "host", linger_ms: 0 } },
      },
    );
    expect(description).toEqual({
      name: "roles",
      apiVersion: 1,
      services: {
        ledger: { methods: ["add"], lifetime: "host", linger: 0 },
        journal: { methods: ["read"] },
      },
      hooks: { guard: "waterfall" },
      contributes: {},
    });
  });
});
