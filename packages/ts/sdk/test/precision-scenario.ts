// Exact WIT u64/s64 across real Orbs: the authored `wasm-precision` provider and its relaying
// consumer, called by the application (application -> source) and by each other
// (source -> source through the captured wire). Shared by the Node and Chrome entry points.
// Uses the `precision` and `precision-relay` fixtures built by `scripts/test-wasm-fixtures.ts`.
import type { Host } from "../src/host.ts";

const MAX = (1n << 64n) - 1n;
const MIN = -(1n << 63n);
const SMAX = (1n << 63n) - 1n;
const NUMBERS = "example:precision/numbers@1.0.0";

/**
 * Compare WIT values structurally. Record fields are named, so object key order is not data:
 * keys are canonicalized recursively. List order, bigints and typed-array element kinds are
 * preserved exactly.
 */
function same(actual: unknown, expected: unknown, what: string): void {
  const canonical = (value: unknown): unknown => {
    if (typeof value === "bigint") return `${value}n`;
    if (ArrayBuffer.isView(value))
      return {
        [value.constructor.name]: [...(value as unknown as Iterable<unknown>)].map(canonical),
      };
    if (Array.isArray(value)) return value.map(canonical);
    if (value && typeof value === "object")
      return Object.fromEntries(
        Object.keys(value)
          .sort()
          .map((key) => [key, canonical((value as Record<string, unknown>)[key])]),
      );
    return value;
  };
  const text = (value: unknown) => JSON.stringify(canonical(value));
  if (text(actual) !== text(expected))
    throw new Error(`${what}: expected ${text(expected)}, got ${text(actual)}`);
}

async function refused(call: () => Promise<unknown>, what: string): Promise<void> {
  const outcome = await call().then(
    () => "accepted",
    (error) => String(error),
  );
  if (!outcome.includes("requires a bigint") && !outcome.includes("out of range"))
    throw new Error(`${what}: expected a 64-bit refusal, got ${outcome}`);
}

/** Install the fixtures into `host` and check every leg; `host` is closed by the caller. */
export async function verifyPrecisionScenario(
  host: Host,
  fixture: (name: string) => Promise<Uint8Array>,
): Promise<string> {
  await host.install({ artifact: await fixture("precision") });
  await host.install({
    artifact: await fixture("precision-relay"),
    requires: { [NUMBERS]: { orb: "precision", apiVersion: 1 } },
  });
  const direct = (await host.get(`precision::${NUMBERS}`, "main")) as any;
  const relayed = (await host.get("relay::example:precision/relay@1.0.0", "main")) as any;
  const report: string[] = [];
  try {
    for (const [name, numbers] of [
      ["application -> source", direct],
      ["application -> source -> source", relayed],
    ] as const) {
      same(await numbers.add(MAX - 1n, 1n), MAX, `${name}: add at the u64 limit`);
      same(await numbers.add(0n, 0n), 0n, `${name}: small values are bigint`);
      same(await numbers.add((1n << 53n) + 1n, 0n), (1n << 53n) + 1n, `${name}: above 2^53`);
      same(await numbers.add(SMAX, MIN + 1n), 0n, `${name}: s64 limits`);
      const sample = {
        id: 1n,
        values: [MIN, -1n, 0n, SMAX],
        maybe: undefined,
        small: 7,
        label: "18446744073709551615",
      };
      const echoed = await numbers.echo(sample, { tag: "signed", val: SMAX }, [MAX, "t"]);
      same(
        echoed,
        {
          tag: "ok",
          val: {
            id: MAX,
            values: [SMAX, 0n, -1n, MIN],
            maybe: SMAX,
            small: 7,
            label: "18446744073709551615:t",
          },
        },
        `${name}: nested record, list, option, variant, tuple and result`,
      );
      same(
        await numbers.echo(sample, { tag: "signed", val: -5n }, [0n, ""]),
        { tag: "err", val: -5n },
        `${name}: s64 result error payload`,
      );
      for (const bad of [1, "1", MAX + 1n, -1n, Object(1n)])
        await refused(() => numbers.add(bad, 0n), `${name}: refused ${String(bad)}`);
      report.push(name);
    }
    // A sum outside u64 is refused by the provider's own export lowering. That is a trap which
    // quarantines the provider revision, so it runs last: once directly, then, on a reloaded
    // provider revision, through the relay.
    for (const [name, numbers] of [
      ["application -> source", direct],
      ["application -> source -> source", relayed],
    ] as const) {
      if (numbers === relayed) await host.reload([{ artifact: await fixture("precision") }]);
      const overflow = await numbers.add(MAX, 1n).then(
        () => "accepted",
        () => "refused",
      );
      same(overflow, "refused", `${name}: u64 overflow refused by the guest`);
    }
  } finally {
    direct.release();
    relayed.release();
  }
  return `exact u64/s64: ${report.join("; ")}`;
}
