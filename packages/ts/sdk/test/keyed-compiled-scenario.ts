// Shared real-artifact scenario for compiled reference views on Node and Chrome. The compiled
// router (`crates/executor-wasm/rust-keyed-router`) acquires counters of the source `keyed`
// provider by key through `example:keyed/counter-ref@1.0.0`; it mirrors
// `crates/executor-wasm/tests/context_acquire_compiled.rs`.
import type { Host } from "../src/host.ts";

const VIEW = "example:keyed/counter-ref@1.0.0";
const ORB = "compiled-keyed-router";
const ROUTER = `${ORB}::example:keyed/compiled-router@1.0.0`;

type Router = {
  route(key: string, other: string, amount: number, context?: unknown): Promise<string>;
  probe(name: string, context?: unknown): Promise<string>;
  release(): void;
};

/**
 * Exercise keyed selection under each root's principal, the acquisition error lane, guest drops,
 * abandoned and cancelled acquisitions, and retained handles within and across roots. The last
 * step traps the router revision on purpose.
 */
export async function verifyKeyedCompiledScenario(
  host: Host,
  fixture: (name: string) => Promise<Uint8Array>,
  packetLimit = false,
): Promise<string> {
  const equal = (actual: unknown, expected: unknown, what: string) => {
    if (actual !== expected)
      throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
  };
  const references = () => host.diagnostics().references;
  await host.install({ artifact: await fixture("keyed") });
  await host.install({
    artifact: await fixture(ORB),
    requires: { [VIEW]: { orb: "keyed", apiVersion: 1 } },
  });
  const first = (await host.get(ROUTER, "first")) as unknown as Router;
  const second = (await host.get(ROUTER, "second")) as unknown as Router;
  try {
    const run = (principal: string, method: "route" | "probe", ...args: unknown[]) =>
      host.root(
        (context) => (first[method] as (...values: unknown[]) => Promise<string>)(...args, context),
        { principal },
      );
    for (const principal of ["alice", "bob"]) {
      equal(
        await run(principal, "route", "a", "b", 2),
        `2,4,2 a@${principal} b@${principal}`,
        `route for ${principal}`,
      );
      equal(references(), 2, `claims after ${principal}'s root`);
    }
    // Unapproved request data takes the WIT error lane and leaves the revision usable.
    const refused = await run("alice", "probe", "denied");
    if (!refused.startsWith("refused:") || !refused.includes("unapproved key"))
      throw new Error(`denied key: ${refused}`);
    equal(await run("alice", "probe", "ok"), "ok@alice", "approved key after a refusal");
    equal(references(), 2, "claims after the error lane");
    const oversized = await run("alice", "probe", "oversized");
    if (
      packetLimit
        ? !oversized.includes("packet limit")
        : oversized !== "accepted" && !oversized.startsWith("refused:")
    )
      throw new Error(`Oversized key did not return a recoverable outcome: ${oversized}`);
    equal(
      await run("alice", "probe", "after-limit"),
      "after-limit@alice",
      "revision stays healthy after a large key",
    );

    // Observe claims before the root ends: root teardown alone must not hide stranded claims.
    await host.root(
      async (context) => {
        await first.route("drop-a", "drop-b", 1, context);
        equal(references(), 2, "claims after guest drops in a live root");
        equal(await first.probe("abandon", context), "ready: false", "abandoned acquisition");
        equal(references(), 2, "claims after an abandoned acquisition");
        equal(
          await first.probe("abandon-wait", context),
          "barrier@alice",
          "abandoned reply finishes during the callback",
        );
        equal(references(), 2, "claims after an undelivered reply");
        equal(await first.probe("keep", context), "keep@alice", "retained acquisition");
        equal(references(), 3, "the retained claim lives with its root");
        equal(await first.probe("retained", context), "keep@alice", "same-root retained use");
      },
      { principal: "alice" },
    );
    equal(references(), 2, "root end expires its retained claim");

    const [outcome, during] = await host.root(
      async (context) => {
        const probe = first.probe("slow", context).then(
          (value) => value,
          (error: Error) => `rejected: ${error.message}`,
        );
        await new Promise((resolve) => setTimeout(resolve, 200));
        context.cancel();
        return [await probe, references()] as const;
      },
      { principal: "alice" },
    );
    if (outcome === "slow@alice") throw new Error("Cancelled acquisition was accepted");
    equal(during, 2, "cancellation left no acquired claim");
    equal(
      await run("alice", "probe", "after-cancel"),
      "after-cancel@alice",
      "revision stays healthy after cancellation",
    );

    // A different root cannot use the reference even while its owning root is still live.
    let entered!: () => void;
    let failed!: (error: unknown) => void;
    let resume!: () => void;
    const ready = new Promise<void>((resolve, reject) => {
      entered = resolve;
      failed = reject;
    });
    const hold = new Promise<void>((resolve) => {
      resume = resolve;
    });
    const active = host
      .root(
        async (context) => {
          equal(await first.probe("keep", context), "keep@alice", "retain in a live root");
          entered();
          await hold;
        },
        { principal: "alice" },
      )
      .catch((error) => {
        failed(error);
        throw error;
      });
    try {
      await ready;
      const crossed = await host
        .root((context) => second.probe("retained", context), { principal: "alice" })
        .then(
          (value) => `accepted: ${value}`,
          (error: Error) => error.message,
        );
      if (!crossed.toLowerCase().includes("belongs to another root"))
        throw new Error(`Expected live cross-root refusal, got ${crossed}`);
      equal(references(), 3, "the owning root still holds its reference");
    } finally {
      resume();
      await active;
    }
    equal(references(), 2, "the owning root releases its reference on completion");
    return "keyed views, error lane, drops, abandonment, cancellation and root-bound handles";
  } finally {
    first.release();
    second.release();
  }
}
