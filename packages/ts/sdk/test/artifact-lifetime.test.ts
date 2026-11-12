import { expect, test } from "bun:test";
import { BrowserExecution } from "../src/interpreter/browser.ts";
import { memoryFilesystem } from "../src/interpreter/filesystem.ts";

/** Give lifecycle tests an owned contract while engine startup deliberately fails. */
function contracts(kind: "source" | "component" = "source", bindings: object = {}) {
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
              bindings,
              importServices: {},
              plan: { orb: { name: "test", apiVersion: 1, services: [], requires: [], hooks: [] } },
            },
          });
        }
        free() {
          freed++;
        }
      },
      // Raw artifacts keep the source path; compiled ones are covered below.
      compiledArtifact: () => false,
      WasmPreparedComponent: class {
        constructor() {
          throw new Error("Prepared components are not expected");
        }
      } as never,
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

test("a source artifact importing a kernel interface is refused and freed", async () => {
  const contract = contracts("source", { "tembra:guest/state@1.0.0": { kind: "kernel" } });
  const backend = new BrowserExecution({}, contract.module);
  await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
    "tembra:guest/state@1.0.0: the kernel serves tembra:guest to compiled components only",
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
        system: ["clocks"],
        filesystem: [{ provider: "data", mount: "/data", writable: false }],
      },
    }),
  ).rejects.toThrow();
  expect(mounted).toBe(1);
  expect(disposed).toBe(1);
  expect(contract.freed()).toBe(1);
});
