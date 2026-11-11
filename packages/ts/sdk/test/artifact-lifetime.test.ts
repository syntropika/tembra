import { afterAll, beforeAll, expect, test } from "bun:test";
import { BrowserExecution } from "../src/interpreter/browser.ts";
import { TrustedCompiler } from "../src/interpreter/component/compiler.ts";
import { memoryFilesystem } from "../src/interpreter/filesystem.ts";

// Bun has no JSPI; these lifecycle tests stop before any glue runs, so markers suffice.
const wasm = WebAssembly as unknown as Record<string, unknown>;
const saved = { Suspending: wasm.Suspending, promising: wasm.promising };
beforeAll(() => {
  wasm.Suspending ??= function Suspending() {};
  wasm.promising ??= () => {};
});
afterAll(() => {
  wasm.Suspending = saved.Suspending;
  wasm.promising = saved.promising;
});

/** A verified-artifact stand-in whose release is counted. */
function contracts(imports: string[] = []) {
  let freed = 0;
  return {
    freed: () => freed,
    module: {
      default: async () => {},
      instrumentCore(): never {
        throw new Error("No core modules expected");
      },
      WasmArtifact: class {
        description() {
          return JSON.stringify({
            name: "test",
            apiVersion: 1,
            kind: "component",
          });
        }
        sourceRuntime() {
          return true;
        }
        component() {
          return new Uint8Array();
        }
        imports() {
          return JSON.stringify(imports);
        }
        contributions() {
          return "{}";
        }
        bind(): number {
          throw new Error("No methods expected");
        }
        validate(): string {
          throw new Error("No values expected");
        }
        request(packet: string) {
          const kind = (JSON.parse(packet) as { kind: string }).kind;
          if (kind === "describe")
            return JSON.stringify({
              ok: { imports: [], exports: [], types: [], javascriptNames: {} },
            });
          return JSON.stringify({
            ok: {
              bindings: {},
              importServices: {},
              plan: {
                orb: {
                  name: "test",
                  apiVersion: 1,
                  services: [],
                  requires: [],
                  hooks: [],
                },
              },
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

/** A trusted compiler whose bindgen module cannot load. */
const unavailable = () =>
  new TrustedCompiler(() => Promise.reject(new Error("compiler unavailable")));

test("an artifact is freed exactly once when a public import cannot be served", async () => {
  const contract = contracts(["wasi:unknown/thing@0.3.0"]);
  const backend = new BrowserExecution({}, contract.module, unavailable());
  await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
    "does not provide wasi:unknown/thing@0.3.0",
  );
  expect(contract.freed()).toBe(1);
});

for (const [limits, refusal] of [
  [{ fuel: 0 }, "limits.fuel must be a positive safe integer"],
  [{ fuel: 1.5 }, "limits.fuel must be a positive safe integer"],
  [{ timeoutMs: -1 }, "limits.timeoutMs must be a positive safe integer"],
  [
    { memoryBytes: Number.MAX_SAFE_INTEGER + 1 },
    "limits.memoryBytes must be a positive safe integer",
  ],
  [{ memoryBytes: 4096 }, "limits.memoryBytes must hold between 1 and 65535 pages"],
] as const)
  test(`limits ${JSON.stringify(limits)} are refused before any guest exists`, async () => {
    const contract = contracts();
    const backend = new BrowserExecution({ limits }, contract.module, unavailable());
    await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(refusal);
    expect(contract.freed()).toBe(1);
  });

test("failed compiler startup closes mounted providers and their contract together", async () => {
  const contract = contracts();
  const filesystem = memoryFilesystem();
  let mounted = 0,
    disposed = 0;
  const backend = new BrowserExecution(
    {
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
    unavailable(),
  );
  await expect(
    backend.materialize({
      artifact: new Uint8Array(),
      grants: {
        system: ["clocks"],
        filesystem: [{ provider: "data", mount: "/data", writable: false }],
      },
    }),
  ).rejects.toThrow("compiler unavailable");
  expect(mounted).toBe(1);
  expect(disposed).toBe(1);
  expect(contract.freed()).toBe(1);
});
