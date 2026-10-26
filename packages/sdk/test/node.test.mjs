import { test } from "node:test";
import assert from "node:assert/strict";
import { createHost } from "../dist/node.js";
import { facet, Code, Reason, Errors, TembraError } from "../dist/index.js";
import { RemoteHost, serve } from "../dist/rpc.js";

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

test("published entrypoints preserve structured failures across RPC and engine effects", async () => {
  const endpoints = [0, 1].map(() => ({ receive() {}, ended() {} }));
  const transports = endpoints.map((endpoint, index) => ({
    send(frame) {
      const copy = frame.slice();
      queueMicrotask(() => endpoints[1 - index].receive(copy));
    },
    listen(receive, ended) {
      Object.assign(endpoint, { receive, ended });
      return () => {
        endpoint.receive = () => {};
      };
    },
    close() {
      for (const endpoint of endpoints) endpoint.ended();
    },
  }));
  const descriptor = {
    ...Errors.Invalid.StateValidation,
    reason: "quota",
    message: "Quota exceeded",
  };
  const host = createHost();
  const server = serve(host, transports[0], { principal: "authenticated" });
  const remote = new RemoteHost(transports[1]);
  try {
    await host.install(
      facet({
        name: "errors",
        apiVersion: 1,
        services: {
          errors: {
            methods: {
              reject() {
                throw new TembraError(descriptor);
              },
            },
          },
        },
      }),
    );
    await assert.rejects(remote.get("missing", "main"), {
      code: Code.Unavailable,
      reason: Reason.UnknownService,
    });
    const ref = await remote.get("errors", "main");
    await assert.rejects(ref.reject(), descriptor);
    await assert.rejects(
      host.install(
        facet({
          name: "failed-effect",
          apiVersion: 1,
          create(self) {
            self.resource("connection", {
              open() {
                throw new TembraError(descriptor);
              },
            });
            return {};
          },
        }),
      ),
      descriptor,
    );
  } finally {
    remote.close();
    server.close();
    await host.close();
  }
});

test("published SDK tooling exports its build interface", async () => {
  const tooling = await import("../dist/tooling/index.js");
  assert.equal(typeof tooling.build, "function");
  assert.equal(tooling.Platform.Embedded, "embedded");
  const config = { builds: {} };
  assert.equal(tooling.defineConfig(config), config);
});
