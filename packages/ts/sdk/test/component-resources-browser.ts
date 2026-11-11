/** Real prepared components exercising native-profile WIT ownership in Chrome. */
import { createHost, type PreparedComponent } from "../src/browser.ts";
import type { Host } from "../src/host.ts";

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
    throw new Error(`${String(error)}; host failures: ${reported.join("; ")}`, { cause: error });
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
