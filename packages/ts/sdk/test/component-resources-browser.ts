/** Real prepared components exercising native-profile WIT ownership in Chrome. */
import { createHost, type PreparedComponent } from "../src/browser.ts";
import type { Host } from "../src/host.ts";
import { encode, type Encoded } from "../src/interpreter/codec.ts";
import type {
  CapturedArguments,
  CapturedInvocation,
  ResourceCapability,
} from "../src/runtime/adapters.ts";
import type { DynamicReference } from "../src/authoring/types.ts";

const SESSIONS = "tembra-test:resources/sessions@1.0.0";
const VIEWS = "tembra-test:resources/views@1.0.0";
const PACE = "tembra-test:resources/pace@1.0.0";
const RELAY = "resource-consumer::tembra-test:resources/relay@1.0.0";
type Reference = { release(): void; [method: string]: any };

function equal(actual: unknown, expected: unknown, label: string): void {
  if (actual !== expected)
    throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
}

async function artifact(name: string): Promise<Uint8Array> {
  const response = await fetch(`/${name}.wasm`);
  if (!response.ok) throw new Error(`Missing ${name}`);
  return new Uint8Array(await response.arrayBuffer());
}

async function using(
  base: object,
  components: PreparedComponent[],
  run: (host: Host, provider: Reference, relay: Reference) => Promise<void>,
): Promise<void> {
  const host = await createHost({ isolated: { ...base, components } });
  let provider: Reference | undefined;
  let relay: Reference | undefined;
  try {
    await host.install({ artifact: await artifact("resource-pace") });
    await host.install({
      artifact: await artifact("resource-provider"),
      requires: { [PACE]: { orb: "resource-pace", apiVersion: 1 } },
    });
    await host.install({
      artifact: await artifact("resource-consumer"),
      requires: Object.fromEntries(
        [SESSIONS, VIEWS].map((binding) => [binding, { orb: "resource-provider", apiVersion: 1 }]),
      ),
    });
    provider = (await host.get(`resource-provider::${SESSIONS}`, "main")) as Reference;
    relay = (await host.get(RELAY, "main")) as Reference;
    await run(host, provider, relay);
  } catch (error) {
    const reported = host.errors().map((failure) => String(failure));
    throw new Error(`${String(error)}; host failures: ${reported.join("; ")}`, {
      cause: error,
    });
  } finally {
    relay?.release();
    provider?.release();
    await host.close();
    for (const [key, value] of Object.entries(host.diagnostics()))
      if (
        [
          "objects",
          "references",
          "calls",
          "roots",
          "resources",
          "tasks",
          "values",
          "pendingJobs",
        ].includes(key)
      )
        equal(value, 0, `${key} after close`);
  }
}

/** Constructors, methods, persistent ownership, exact integers and callback-bound drops. */
export async function verifyBrowserResourceOwnership(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    const pair = await relay.pair(4n, 2n);
    for (const [index, expected] of [6n, 4n, 3n].entries())
      equal(pair[index], expected, `independent session ${index}`);
    equal(await relay.closed(), 3n, "each constructor/factory result destroyed once");
    equal(await relay.keep(10n), 10n, "retain imported owner");
    equal(await relay.bump(5n), 15n, "retained owner across roots");
    equal(await relay.closed(), 3n, "retained owner stays alive");
    await relay.discard();
    equal(await relay.closed(), 4n, "retained owner destroyed once");
    equal(await relay.paced(3n, 2n), 23n, "borrow spans an async host import");
    equal(await relay.closed(), 5n, "async borrow settles before destruction");
    equal(await relay.describe(), "views", "aliased resource interface");

    const first = ["personal ✓", "decoder/🎧 \\"];
    const second = ["work", "other"];
    const seed = (1n << 64n) - 11n;
    const report = await relay.keyed(first, second, seed);
    for (let index = 0; index < 3; index++) {
      const expected = index < 2 ? first : second;
      equal(report.sessions[index].key[0], expected[0], "typed key first field");
      equal(report.sessions[index].key[1], expected[1], "typed key second field");
      equal(report.sessions[index].total, seed + BigInt(index + 1), "exact u64 session total");
    }
    equal(report.closedBeforeSettlement, 5n, "guest drops wait for callback settlement");
    equal(await provider.closed(), 8n, "all three keyed sessions destroyed exactly once");
    equal(host.errors().length, 0, "no hidden lifecycle failures");
  });
}

/** Data-only host calls cannot acquire or forge a WIT owner. */
export async function verifyBrowserResourceRefusal(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider) => {
    const outcome = await provider["[constructor]session"](12n).catch((error: unknown) => error);
    if (!(outcome instanceof Error)) throw new Error("Data-only caller received a resource");
    equal(await provider.closed(), 1n, "refused result physically destroyed once");
    const forged = await provider["[method]session.total"]().catch((error: unknown) => error);
    if (!(forged instanceof Error)) throw new Error("Method accepted no resource receiver");
    equal(
      await provider.closed(),
      1n,
      "invalid receiver never entered or quarantined the provider",
    );
    equal(await provider.label(), "sessions", "provider remains usable after refusal");
    equal(host.errors().length, 0, "refusal leaves no cleanup error");
  });
}

/** Reload cannot redirect an owner issued by the previous component instance. */
export async function verifyBrowserResourceReload(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    await relay.keep(9n);
    await host.reload([
      {
        artifact: await artifact("resource-provider"),
        requires: { [PACE]: { orb: "resource-pace", apiVersion: 1 } },
      },
    ]);
    const stale = await relay.bump(1n).catch((error: unknown) => error);
    if (!(stale instanceof Error)) throw new Error("Old owner reached a replacement instance");
    equal(await provider.closed(), 0n, "replacement received no old resource");
    equal(await provider.label(), "sessions", "replacement is not quarantined by stale owner");
  });
}

/** Nested WIT result/record/option/list values carry nominal owners and exact 64-bit metadata. */
export async function verifyBrowserNestedResources(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    const seed = (1n << 64n) - 11n;
    const totals = await relay.nested(seed, false);
    for (const [index, expected] of [seed, seed + 1n, seed + 2n, seed + 3n, 1n, seed].entries())
      equal(totals[index], expected, `nested resource ${index}`);
    equal(totals.length, 6, "all nested resources were callable");
    equal(await provider.closed(), 4n, "nested session owners destroyed exactly once");
    const empty = await relay.nested(0n, false);
    equal(empty.length, 3, "none and empty list carry no resource");
    equal(empty[0], 0n, "primary with no optional session");
    equal(await provider.closed(), 5n, "only primary was allocated");
    equal((await relay.nested(1n, true)).length, 0, "err result carries no resource");
    equal(await provider.closed(), 5n, "err result allocated no resource");
    const refused = await provider["open-bundle"](seed, false).catch((error: unknown) => error);
    if (!(refused instanceof Error))
      throw new Error("Data caller received nested resource authority");
    equal(await provider.closed(), 9n, "refused nested batch physically destroyed");
    for (const [value, expected, destroyed] of [
      [seed, [seed, seed, seed + 1n], 11n],
      [0n, [0n, 0n, 0n], 12n],
    ] as const) {
      const result = await relay["nested-shapes"](value);
      equal(result.length, expected.length, "tuple and variant result length");
      for (const [index, value] of expected.entries())
        equal(result[index], value, `tuple/variant item ${index}`);
      equal(await provider.closed(), destroyed, "tuple and variant owners destroyed exactly once");
    }
    equal(host.errors().length, 0, "nested result leaves no lifecycle failures");
  });
}

/** Canonical own parameters move whole and nested owners back to their issuing instance. */
export async function verifyBrowserResourceArguments(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    const seed = (1n << 64n) - 11n;
    const values = await relay["move-back"](seed);
    const expected = [seed, seed, seed + 1n, seed + 2n, seed + 3n, 1n, seed];
    equal(values.length, expected.length, "all moved owners were consumed");
    for (const [index, value] of expected.entries())
      equal(values[index], value, `moved owner ${index}`);
    equal(await provider.closed(), 5n, "single and nested moved owners destroyed once");
    const empty = await relay["move-back"](0n);
    for (const [index, value] of [0n, 0n, 1n, 0n].entries())
      equal(empty[index], value, `empty batch ${index}`);
    equal(empty.length, 4, "none and empty lists move no attachments");
    equal(await provider.closed(), 7n, "empty batch owns only its two sessions");
    equal(
      await relay["move-later"](23n),
      23n,
      "owned argument survives real asynchronous guest work",
    );
    equal(await provider.closed(), 8n, "asynchronously consumed owner destroyed once");
    const refused = await provider.take(0).catch((error: unknown) => error);
    if (!(refused instanceof Error)) throw new Error("Data caller forged owned argument authority");
    equal(await provider.label(), "sessions", "data argument refusal preserves provider");
    equal(host.errors().length, 0, "moves leave no hidden lifecycle failures");
  });
}

/** Exercise the same private lane as compiled imports; no owner enters application data. */
async function captured(
  host: Host,
  reference: Reference,
  method: string,
  input: Encoded,
  arguments_?: CapturedArguments,
): Promise<CapturedInvocation> {
  const session = host.session;
  const record = session.handles.reference(reference as DynamicReference);
  const environment = session.objects.get(record.object)?.versions.values().next().value;
  const signature = environment?.orb.info.services.find(
    (service) => service.name === record.address.service,
  )?.signature;
  if (!signature) throw new Error("Missing actual prepared provider signature");
  const capture: CapturedInvocation = {
    signature,
    input,
    ...(arguments_ ? { arguments: arguments_ } : {}),
  };
  await host.root((context) => session.calls.invokePhysical(record, method, [], context, capture));
  return capture;
}

function transfer(owner: ResourceCapability): CapturedArguments {
  return {
    owners: [owner],
    accepted: false,
    accept() {
      Object.assign(this, { accepted: true });
    },
  };
}

async function opened(host: Host, provider: Reference, seed: bigint): Promise<ResourceCapability> {
  const result = await captured(
    host,
    provider,
    "open",
    encode([seed], undefined, { items: ["u64"] }),
  );
  const owner = result.capabilities?.[0];
  if (owner?.kind !== "resource") throw new Error("Missing real prepared resource owner");
  return owner;
}

/** A foreign alias retains its original claim and its recipient's ownership across roots. */
export async function verifyBrowserForeignResourceArguments(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    const seed = (1n << 64n) - 11n;
    const original = await opened(host, provider, seed);
    const moved = original.move();
    const claim = moved.reference;
    let stale = false;
    try {
      Reflect.get(original, "reference");
    } catch {
      stale = true;
    }
    equal(stale, true, "sender aliases revoked immediately");
    const batch = transfer(moved);
    try {
      const references = host.diagnostics().references;
      await captured(host, relay, "accept-keep", encode([0]), batch);
      equal(batch.accepted, true, "foreign recipient accepted ownership");
      original.close();
      equal(moved.reference, claim, "foreign acceptance preserves original provider claim");
      equal(host.diagnostics().references, references, "foreign acceptance does not reanchor");
      equal(await relay.bump(5n), seed + 5n, "recipient keeps owner across roots");
      equal(await provider.closed(), 0n, "stale sender drop preserves moved owner");
      await relay.discard();
      equal(await provider.closed(), 1n, "recipient drop destroys moved owner once");
      equal(host.errors().length, 0, "foreign move leaves no lifecycle failure");
    } finally {
      if (!batch.accepted) moved.close();
    }
  });
}

/** Issuer retirement follows physical state after its original wrapper has been moved away. */
export async function verifyBrowserMovedResourceReload(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    const original = await opened(host, provider, 50n);
    const moved = original.move();
    const batch = transfer(moved);
    try {
      await captured(host, relay, "accept-keep", encode([0]), batch);
      await host.reload([
        {
          artifact: await artifact("resource-provider"),
          requires: { [PACE]: { orb: "resource-pace", apiVersion: 1 } },
        },
      ]);
      const stale = await relay.bump(1n).catch((error: unknown) => error);
      if (!(stale instanceof Error)) throw new Error("Moved owner reached replacement issuer");
      equal(await provider.closed(), 0n, "replacement issuer received no stale resource");
      equal(await provider.label(), "sessions", "replacement issuer remains healthy");
    } finally {
      if (!batch.accepted) moved.close();
    }
  });
}

/** Moving cannot broaden the original provider grant; a trapped recipient releases accepted ownership. */
export async function verifyBrowserMovedResourceRefusal(
  base: object,
  components: PreparedComponent[],
): Promise<void> {
  await using(base, components, async (host, provider, relay) => {
    const restricted = (await host.get(`resource-provider::${SESSIONS}`, "main", {
      methods: ["open"],
    })) as Reference;
    try {
      const original = await opened(host, restricted, 31n);
      equal(
        original.reference.methods.has("[method]session.total"),
        false,
        "owner grant has no total method",
      );
      const moved = original.move();
      const batch = transfer(moved);
      try {
        const denied = await captured(host, relay, "accept", encode([0]), batch).catch(
          (error: unknown) => error,
        );
        if (!(denied instanceof Error))
          throw new Error("Foreign recipient broadened owner authority");
        equal(batch.accepted, true, "guest failed after ownership acceptance");
        while (host.session.jobs.size) await Promise.allSettled([...host.session.jobs]);
        equal(await provider.closed(), 1n, "trapped recipient releases moved owner once");
        equal(await provider.label(), "sessions", "foreign trap preserves original provider");
      } finally {
        if (!batch.accepted) moved.close();
      }
    } finally {
      restricted.release();
    }
  });
}
