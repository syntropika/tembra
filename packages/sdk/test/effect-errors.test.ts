import { expect, spyOn, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { facet } from "../src/facet.ts";
import { TembraError } from "../src/error.ts";
import { Errors } from "../src/errors.ts";
import { KernelOp } from "../src/protocol.ts";

for (const kind of ["unprintable", "oversized", "hostile", "unicode"] as const)
  test(`effect acknowledgement survives ${kind} errors and preserves successful sibling cleanup`, async () => {
    const host = createHost();
    const sizes: number[] = [];
    const port = host.session.kernel.port;
    const request = port.request.bind(port);
    const commands = spyOn(port, "request").mockImplementation((metadata) => {
      if (JSON.parse(metadata).op === KernelOp.ProgramCompleteStep)
        sizes.push(new TextEncoder().encode(metadata).length);
      return request(metadata);
    });
    const huge = kind === "unicode" ? "\uD800" + "😀".repeat(200_000) : "\u0000😀".repeat(200_000);
    const error =
      kind === "unprintable"
        ? Object.create(null)
        : new TembraError({ ...Errors.Unavailable.Callback, reason: huge }, huge);
    if (kind === "hostile")
      Object.defineProperty(error, "message", {
        get() {
          throw null;
        },
      });
    let closed = 0;
    await host.install(
      facet({
        name: "effect-errors",
        apiVersion: 1,
        services: {
          item: {
            create(self) {
              self.resource("successful", {
                open: () => ({}),
                close() {
                  closed++;
                },
              });
              for (let index = 0; index < 64; index++)
                self.resource(`failed-${index}`, {
                  open() {
                    throw error;
                  },
                });
              return {};
            },
            methods: {},
          },
        },
      }),
    );
    try {
      await expect(host.get("item", "main")).rejects.toMatchObject({ code: "unavailable" });
      await host.close();
      expect(closed).toBe(1);
      expect(Math.max(...sizes)).toBeLessThan(1024 * 1024);
      expect(host.diagnostics()).toMatchObject({
        objects: 0,
        resources: 0,
        pendingJobs: 0,
        values: 0,
      });
    } finally {
      commands.mockRestore();
    }
  });
