// Exact u64/s64 across the TypeScript inter-Orb edges, through the real kernel, SDK roots,
// `Revision` and `Scope`. The contract answers (real `describe` graphs, implementation metadata
// shaped as the Rust planner emits it: signed Orb requirements, full hook interface identities)
// and the guest ports are test doubles at the backend seam: a port speaks the guest's wire form,
// so 64-bit values reach it only as canonical decimal strings. Non-captured providers are modelled
// as a prepared provider (internal adapted signature metadata) behind a native adapter double or
// a host handler; a public plain JavaScript Orb cannot declare a signature and so cannot satisfy
// a signed WIT import.
import { expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { NodeContract } from "@syntropika/tembra-engine-node";
import { isolate, signature } from "#tembra-adapters";
import type { NativeAdapter } from "../src/authoring/types.ts";
import { createHost } from "../src/node.ts";
import { bail } from "../src/index.ts";
import { reply } from "../src/interpreter/codec.ts";
import { Revision } from "../src/interpreter/revision.ts";
import type {
  ContractPort,
  ExecutionScope,
  InstallationPlan,
  InterpreterPort,
} from "../src/interpreter/types.ts";

const MAX = (1n << 64n) - 1n;
const MIN = -(1n << 63n);
const NUMBERS = "example:precision/numbers";
/** Full hook interface identities, as the planner names hook and contribution bindings. */
const TALLY = "example:precision/tally";
const COUNT = "example:precision/count";
const WIT = `package example:precision;
interface numbers {
  add: func(a: u64, b: s64) -> u64;
  plain: func(x: u32) -> u32;
}
interface relay { relay: func(a: u64, b: s64) -> u64; }
interface tally { tally: func(value: u64) -> option<u64>; }
interface count { count: func(value: s64) -> option<s64>; }
world provider { export numbers; }
world consumer { import numbers; export relay; export tally; }
world contributor { export count; }`;

const directory = await mkdtemp(path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "wit-"));
await writeFile(path.join(directory, "contract.wit"), WIT);
const graph = (world: string) =>
  JSON.parse(new NodeContract(directory, world).request('{"kind":"describe"}')).ok;
const graphs = {
  provider: graph("provider"),
  consumer: graph("consumer"),
  contributor: graph("contributor"),
};
await rm(directory, { recursive: true, force: true });

const orb = (name: string, requires: InstallationPlan["plan"]["orb"]["requires"] = []) => ({
  name,
  apiVersion: 1,
  services: [],
  requires,
  hooks: [],
  hookSignatures: {},
});
const service = (orbName: string, alias: string, contract: string, methods: string[]) => ({
  name: `${orbName}::${contract}`,
  interface: contract,
  alias,
  methods: Object.fromEntries(methods.map((method) => [method, method])),
  lifetime: "referenced",
  linger: 0,
  signature: `sig-${alias}`,
});

/** The contract module's answers; every value check is accepted and recorded. */
function contract(
  describe: unknown,
  implementation: unknown,
): ContractPort & { checked: string[] } {
  let slot = 0;
  const checked: string[] = [];
  return {
    checked,
    bind: () => ++slot,
    validate: (_slot, _arguments, value) => {
      checked.push(value);
      return '{"ok":null}';
    },
    request: (packet) =>
      JSON.stringify({ ok: JSON.parse(packet).kind === "describe" ? describe : implementation }),
  };
}

type Guest = {
  create?(service: string | null, scope: ExecutionScope): void;
  invoke?(method: string, args: unknown[], scope: ExecutionScope): Promise<string>;
  contribute?(args: unknown[]): string;
};
/** A guest double speaking the wire form; it records every packet's exact text. */
function port(guest: Guest): InterpreterPort & { packets: string[] } {
  const packets: string[] = [];
  return {
    packets,
    description: () => "{}",
    declare(packet, scope) {
      const request = JSON.parse(packet);
      if (request.kind === "create") guest.create?.(request.service, scope);
      return null;
    },
    async execute(packet, scope) {
      packets.push(packet);
      const request = JSON.parse(packet);
      if (request.kind === "contribute") return guest.contribute!(request.args);
      return guest.invoke!(request.method, request.args, scope);
    },
    release() {},
    close() {},
  };
}

/** A host whose isolated revisions are selected by the artifact's first byte. */
function host(
  revisions: (() => {
    port: InterpreterPort;
    contract: ContractPort;
    name: string;
    installation: InstallationPlan;
  })[],
  native?: NativeAdapter,
) {
  return createHost({
    principal: "test",
    ...(native ? { native } : {}),
    ...{
      [isolate]: async (definition: { artifact: Uint8Array }) => {
        const selected = revisions[definition.artifact[0]!]!();
        return new Revision(selected.port, selected.contract, 5000).definition(
          { name: selected.name, apiVersion: 1 },
          selected.installation,
        );
      },
    },
  });
}

const provider = (entered: string[]) => {
  const guest = port({
    async invoke(method, args) {
      entered.push(method);
      const [a, b] = args as [string, string];
      // The guest sees wire text; `BigInt` of canonical text is exact.
      return JSON.stringify(String(BigInt(a) + BigInt(b)));
    },
  });
  return {
    guest,
    revision: () => ({
      port: guest,
      contract: contract(graphs.provider, {
        services: [service("provider", "numbers", NUMBERS, ["add", "plain"])],
        hooks: { hooks: [], contributions: [] },
        plan: { orb: orb("provider") },
      }),
      name: "provider",
      installation: { bindings: {}, importServices: {}, plan: { orb: orb("provider") } },
    }),
  };
};

/**
 * The consumer relays its wire arguments to its numbers import and runs its `tally` hook. Its
 * requirement is signed, as the planner records for every Orb binding.
 */
const consumer = (target: string, tallies: string[]) => {
  let reference = 0;
  let hook = 0;
  const guest = port({
    create(name, scope) {
      if (name !== "relay") return;
      reference = scope.sync({
        operation: "reference.declare",
        service: NUMBERS,
        key: ["main"],
      }) as number;
      hook = scope.sync({ operation: "hook.declare", name: "tally" }) as number;
    },
    async invoke(_method, args, scope) {
      const sum = await scope.async({
        operation: "reference.call",
        id: reference,
        method: "add",
        args,
      });
      const tallied = await scope.async({ operation: "hook.run", id: hook, value: args[0] });
      tallies.push(reply(tallied));
      return reply(sum);
    },
  });
  const requirement = {
    orb: target,
    apiVersion: 1,
    services: { [`${target}::${NUMBERS}`]: ["add", "plain"] },
    signatures: { [`${target}::${NUMBERS}`]: "sig-numbers" },
    hooks: [],
    hookSignatures: {},
  };
  const plan = { orb: orb("consumer", [requirement]) };
  return {
    guest,
    revision: () => ({
      port: guest,
      contract: contract(graphs.consumer, {
        services: [service("consumer", "relay", "example:precision/relay", ["relay"])],
        hooks: {
          hooks: [
            {
              name: TALLY,
              alias: "tally",
              method: "tally",
              slot: 900,
              signature: "sig-tally",
              composition: "bail",
            },
          ],
          contributions: [],
        },
        plan,
      }),
      name: "consumer",
      installation: {
        bindings: { [NUMBERS]: { kind: "orb" as const, name: target, api_version: 1 } },
        importServices: { [NUMBERS]: `${target}::${NUMBERS}` },
        plan,
      },
    }),
  };
};

const settle = async (served: ReturnType<typeof createHost>) => {
  await served.close();
  const diagnostics = served.diagnostics();
  for (const name of ["objects", "references", "roots", "calls", "values", "pendingJobs"] as const)
    expect(diagnostics[name]).toBe(0);
};

test("application calls into an isolated Orb: bigint in and out, wrong kinds refused before entry", async () => {
  const entered: string[] = [];
  const { guest, revision } = provider(entered);
  const served = host([revision]);
  try {
    await served.install({ artifact: Uint8Array.of(0) });
    const numbers = (await served.get(`provider::${NUMBERS}`, "main")) as any;
    expect(await numbers.add(MAX - 1n, 1n)).toBe(MAX);
    expect(await numbers.add(0n, 0n)).toBe(0n);
    expect(guest.packets.at(-2)).toContain('"args":["18446744073709551614","1"]');
    for (const bad of [
      [1, 0n],
      ["1", 0n],
      [MAX + 1n, 0n],
      [0n, MIN - 1n],
      [Object(1n), 0n],
    ])
      await expect(numbers.add(...bad)).rejects.toThrow(/Portable value rejected/);
    expect(entered).toEqual(["add", "add"]);
    numbers.release();
  } finally {
    await settle(served);
  }
});

/** A JavaScript hook participant contributing to the consumer's `tally` hook. */
const tallier = (received: unknown[]) => ({
  name: "tallier",
  apiVersion: 1,
  requires: { consumer: { apiVersion: 1, hooks: [TALLY] } },
  contributes: {
    consumer: {
      [TALLY]: (_context: unknown, value: unknown) => {
        received.push(value);
        return value;
      },
    },
  },
});

test("captured source->source keeps the exact wire text; a guest hook gives its JavaScript participant bigint", async () => {
  const entered: string[] = [];
  const tallies: string[] = [];
  const participant: unknown[] = [];
  const { guest: target, revision: providing } = provider(entered);
  const { revision: consuming } = consumer("provider", tallies);
  const served = host([providing, consuming]);
  try {
    await served.install({ artifact: Uint8Array.of(0) });
    await served.install([{ artifact: Uint8Array.of(1) }, tallier(participant)]);
    const relay = (await served.get("consumer::example:precision/relay", "main")) as any;
    expect(await relay.relay(MAX - 2n, 2n)).toBe(MAX);
    // No JavaScript value existed in between: the provider got the consumer's text verbatim.
    expect(target.packets.at(-1)).toContain('"args":["18446744073709551613","2"]');
    // The hook participant saw bigint; its bigint result reached the guest as canonical text.
    expect(participant).toEqual([MAX - 2n]);
    expect(tallies).toEqual(['"18446744073709551613"']);
    relay.release();
  } finally {
    await settle(served);
  }
});

/** A prepared provider: its service carries checked signature metadata, as an adapter registers it. */
const prepared = (add: (...args: unknown[]) => unknown) => ({
  name: "numbers-native",
  apiVersion: 1,
  services: {
    [`numbers-native::${NUMBERS}`]: {
      [signature]: "sig-numbers",
      methods: { add, plain: () => 0 },
    },
  },
});

/** A native adapter double owning the prepared provider; it records the arguments it receives. */
function nativeAdapter(received: unknown[][], owner: () => number | undefined): NativeAdapter {
  return {
    hasOrb: (id) => id === owner(),
    async activate() {},
    async invoke(_invocation, args) {
      received.push(["native adapter", ...args]);
      return (args[0] as bigint) + (args[1] as bigint);
    },
    async dispose() {},
    async effect() {},
    value() {
      throw new Error("no native values in this test");
    },
  };
}

test("signed plan, non-captured provider: the native adapter and a host handler get bigint on every route", async () => {
  for (const route of ["native adapter", "host handler"] as const) {
    const received: unknown[][] = [];
    const tallies: string[] = [];
    let served: ReturnType<typeof createHost>;
    const owner = () => served.installedOrbs().find((info) => info.name === "numbers-native")?.id;
    const { revision: consuming } = consumer("numbers-native", tallies);
    served = host(
      [consuming],
      route === "native adapter" ? nativeAdapter(received, owner) : undefined,
    );
    try {
      await served.install([
        prepared((_environment, _context, a, b) => {
          received.push(["host handler", a, b]);
          return (a as bigint) + (b as bigint);
        }),
        { artifact: Uint8Array.of(0) },
      ]);
      // Import route: the signature exists, but this provider is not a captured method.
      const relay = (await served.get("consumer::example:precision/relay", "main")) as any;
      expect(await relay.relay(MAX, MIN)).toBe(MAX + MIN);
      // Direct route: the application calls the same service.
      const numbers = (await served.get(`numbers-native::${NUMBERS}`, "main")) as any;
      expect(await numbers.add(MAX, MIN)).toBe(MAX + MIN);
      // One JavaScript domain form on both routes: bigint, never wire strings.
      expect(received).toEqual([
        [route, MAX, MIN],
        [route, MAX, MIN],
      ]);
      relay.release();
      numbers.release();
    } finally {
      await settle(served);
    }
  }
});

test("JavaScript hook -> isolated contribution: typed both ways, wrong kinds refused", async () => {
  const contributed: unknown[][] = [];
  const guest = port({
    contribute(args) {
      contributed.push(args);
      return JSON.stringify(String(BigInt(args[0] as string) * 2n));
    },
  });
  const plan = {
    orb: orb("contributor", [
      {
        orb: "counter",
        apiVersion: 1,
        services: {},
        signatures: {},
        hooks: [COUNT],
        hookSignatures: {},
      },
    ]),
  };
  const served = host([
    () => ({
      port: guest,
      contract: contract(graphs.contributor, {
        services: [],
        hooks: {
          hooks: [],
          contributions: [
            {
              binding: "counter",
              target: "counter",
              name: COUNT,
              alias: "count",
              method: "count",
              slot: 901,
              signature: "sig-count",
              priority: 0,
              optional: true,
            },
          ],
        },
        plan,
      }),
      name: "contributor",
      installation: { bindings: {}, importServices: {}, plan },
    }),
  ]);
  try {
    await served.install({
      name: "counter",
      apiVersion: 1,
      hooks: { [COUNT]: bail() },
      services: {
        counter: {
          create: (self: any) => ({ count: self.hook(COUNT) }),
          methods: {
            run: ({ count }: any, context: unknown, value: unknown) => count.run(context, value),
          },
        },
      },
    });
    await served.install({ artifact: Uint8Array.of(0) });
    const counter = (await served.get("counter", "main")) as any;
    expect(await counter.run(MIN / 2n)).toBe(MIN);
    expect(contributed).toEqual([["-4611686018427387904"]]);
    await expect(counter.run(-7)).rejects.toThrow(/s64 requires a bigint/);
    expect(contributed.length).toBe(1);
    counter.release();
  } finally {
    await settle(served);
  }
});
