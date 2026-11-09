import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";

test("artifact URLs report HTTP failures before attempting to decode response bodies", async () => {
  const server = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch: () => new Response("Service unavailable", { status: 503 }),
  });
  const host = createHost();
  try {
    await expect(host.load(new URL("orb.wasm", server.url))).rejects.toThrow(
      "Orb request failed: 503",
    );
    expect(host.diagnostics().orbs).toBe(0);
  } finally {
    await host.close();
    server.stop(true);
  }
});
