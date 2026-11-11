// Shared real-artifact scenario for source Context.get on Node and Chrome.
import type { Host } from "../src/host.ts";

/** Exercise per-request keys, principal isolation, root expiry and readiness cancellation. */
export async function verifyContextAcquireScenario(
  host: Host,
  fixture: (name: string) => Promise<Uint8Array>,
): Promise<string> {
  const equal = (actual: unknown, expected: unknown) => {
    if (actual !== expected) throw new Error(`Expected ${String(expected)}, got ${String(actual)}`);
  };
  await host.install({ artifact: await fixture("keyed") });
  await host.install({
    artifact: await fixture("keyed-router"),
    requires: { "example:keyed/counter@1.0.0": { orb: "keyed", apiVersion: 1 } },
  });
  const router = (await host.get("router::example:keyed/router@1.0.0", "main")) as any;
  try {
    const run = (principal: string, method: string, ...args: unknown[]) =>
      host.root((context) => router[method](...args, context), { principal });
    equal(await run("alice", "route", "a", "b", 2), "2,4,2 a@alice b@alice");
    equal(await run("bob", "route", "a", "b", 2), "2,4,2 a@bob b@bob");
    equal(host.diagnostics().references, 1);
    for (const [probe, refusal] of [
      ["retained-context", "refused: context callback has ended"],
      ["retained-reference", "belongs to another root"],
      ["unknown-import", "not a selected wit orb import"],
      ["key-kind", "a reference key is a string or a list of strings"],
      ["release", "released"],
    ] as const) {
      const result = String(await run("alice", "probe", probe)).toLowerCase();
      if (!result.includes(refusal)) throw new Error(`${probe}: ${result}`);
    }
    equal(host.diagnostics().references, 1);
    const [outcome, during] = await host.root(
      async (context) => {
        const probe = (router.probe("slow", context) as Promise<string>).then(
          (value) => value,
          (error: Error) => `rejected: ${error.message}`,
        );
        await new Promise((resolve) => setTimeout(resolve, 500));
        context.cancel();
        return [await probe, host.diagnostics().references] as const;
      },
      { principal: "alice" },
    );
    if (outcome === "accepted") throw new Error("Cancelled acquisition was accepted");
    equal(during, 1);
    return "per-request keys, principals, expired references and cancelled readiness";
  } finally {
    router.release();
  }
}
