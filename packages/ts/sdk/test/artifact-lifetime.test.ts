import { expect, test } from "bun:test";
import { BrowserExecution } from "../src/interpreter/browser.ts";
import { memoryFilesystem } from "../src/interpreter/filesystem.ts";

/** Give lifecycle tests an owned contract while engine startup deliberately fails. */
function contracts(kind: "source" | "component" = "source") {
  let freed = 0;
  return {
    freed: () => freed,
    module: {
      default: async () => {},
      WasmArtifact: class {
        description() {
          return JSON.stringify({ name: "test", apiVersion: 1, kind });
        }
        source() {
          return "";
        }
        bind(): number {
          throw new Error("No methods expected");
        }
        validate(): string {
          throw new Error("No values expected");
        }
        request() {
          return JSON.stringify({
            ok: {
              bindings: {},
              importServices: {},
              plan: { orb: { name: "test", apiVersion: 1, services: [], requires: [], hooks: [] } },
            },
          });
        }
        free() {
          freed++;
        }
      },
    },
  };
}

for (const kind of ["source", "component"] as const)
  test(`a ${kind} artifact is freed exactly once after failed preparation`, async () => {
    const contract = contracts(kind);
    const backend = new BrowserExecution({ limits: { memoryBytes: 1 } }, contract.module);
    await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
      kind === "component"
        ? "Compiled Orb execution is unavailable"
        : "Invalid interpreter memory bound",
    );
    expect(contract.freed()).toBe(1);
  });

test("failed engine startup closes mounted providers and their contract together", async () => {
  const contract = contracts();
  const filesystem = memoryFilesystem();
  let mounted = 0,
    disposed = 0;
  const backend = new BrowserExecution(
    {
      engine: "invalid URL",
      providers: {
        filesystem: {
          data: {
            open(mount, writable) {
              mounted++;
              const files = filesystem.open(mount, writable);
              return {
                ...files,
                async dispose() {
                  disposed++;
                  await files.dispose();
                },
              };
            },
          },
        },
      },
    },
    contract.module,
  );
  await expect(
    backend.materialize({
      artifact: new Uint8Array(),
      grants: {
        filesystem: [{ provider: "data", mount: "/data", writable: false }],
      },
    }),
  ).rejects.toThrow();
  expect(mounted).toBe(1);
  expect(disposed).toBe(1);
  expect(contract.freed()).toBe(1);
});
