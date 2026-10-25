import { test } from "node:test";
import assert from "node:assert/strict";
import { createHost } from "../dist/node.js";
import { facet } from "../dist/index.js";

test("published JS entrypoints execute on Node with the actual Node-API addon", async () => {
  const host = createHost();
  await host.install(
    facet({
      name: "counter",
      apiVersion: 1,
      services: {
        counter: {
          create(self) {
            return { state: self.state({ value: 0 }) };
          },
          methods: {
            add({ state }, ctx, value) {
              return state.update(ctx, (current) => ({ value: current.value + value })).value;
            },
          },
        },
      },
    }),
  );
  const reference = await host.get("counter", "main");
  assert.equal(await reference.add(2), 2);
  assert.equal(reference.state.value.value, 2);
  reference.release();
  await host.close();
  assert.equal(host.diagnostics().values, 0);
});
