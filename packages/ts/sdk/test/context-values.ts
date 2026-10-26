import { ContextKey } from "../src/index.ts";
import type { Host } from "../src/host.ts";

const Trace = ContextKey.text("browser/trace");
const Count = ContextKey.integer("browser/count");
const Flag = ContextKey.boolean("browser/flag");

/** Verify per-call scalar metadata through the actual browser engine. */
export async function verifyContextValues(host: Host): Promise<void> {
  await host.install({
    name: "browser-context",
    apiVersion: 1,
    services: {
      "browser-context": {
        methods: {
          read(_, ctx) {
            return [ctx.value(Trace), ctx.value(Count), ctx.value(Flag)];
          },
        },
      },
    },
  });
  const reference = await host.get("browser-context", "main");
  try {
    await host.root(async (root) => {
      const left = root.with(Trace, "left").with(Count, Number.MAX_SAFE_INTEGER);
      const right = root.with(Trace, "right").with(Flag, true);
      const values = await Promise.all([reference.read(left), reference.read(right)]);
      if (
        JSON.stringify(values) !==
        JSON.stringify([
          ["left", Number.MAX_SAFE_INTEGER, null],
          ["right", null, true],
        ])
      )
        throw new Error("Browser context branches leaked or lost precision");
      if (root.value(Trace) !== undefined || left.value(Trace) !== "left")
        throw new Error("Browser derivation modified its parent or sibling");
      const portable = JSON.parse(JSON.stringify(left.exportValues([Trace, Count])));
      if (
        portable["browser/trace@1"] !== "left" ||
        portable["browser/count@1"] !== Number.MAX_SAFE_INTEGER
      )
        throw new Error("Browser metadata export changed the scalar contract");
    });
  } finally {
    reference.release();
  }
}
