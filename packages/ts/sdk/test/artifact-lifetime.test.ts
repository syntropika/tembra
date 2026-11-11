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
function contracts(imports: string[] = [], shared = false) {
  let freed = 0;
  let released = 0;
  let runtimes = 0;
  return {
    freed: () => freed,
    released: () => released,
    runtimes: () => runtimes,
    module: {
      default: async () => {},
      instrumentCore(): never {
        throw new Error("No core modules expected");
      },
      WasmSharedRuntime: class {
        constructor(bytes: Uint8Array) {
          if (bytes[0] !== 1) throw new Error("Runtime identity differs");
          runtimes++;
        }
        free() {}
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
        requiresRuntime() {
          return shared;
        }
        component() {
          return new Uint8Array();
        }
        expandedComponent(runtime: { free(): void }) {
          if (!runtime) throw new Error("Shared runtime is missing");
          return this.component();
        }
        releaseComponent() {
          released++;
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

test("concurrent source installations load and check one shared runtime", async () => {
  const contract = contracts([], true);
  let loads = 0;
  const backend = new BrowserExecution({}, contract.module, unavailable(), async () => {
    loads++;
    return Uint8Array.of(1);
  });
  const results = await Promise.allSettled([
    backend.materialize({ artifact: new Uint8Array() }),
    backend.materialize({ artifact: new Uint8Array() }),
  ]);
  for (const result of results) {
    expect(result.status).toBe("rejected");
    if (result.status === "rejected")
      expect(String(result.reason)).toContain("compiler unavailable");
  }
  expect(loads).toBe(1);
  expect(contract.runtimes()).toBe(1);
  expect(contract.freed()).toBe(2);
});

test("a failed runtime load is retried without retaining its artifacts", async () => {
  const contract = contracts([], true);
  let loads = 0;
  const backend = new BrowserExecution({}, contract.module, unavailable(), async () => {
    if (++loads === 1) throw new Error("Runtime offline");
    return Uint8Array.of(1);
  });
  await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
    "Runtime offline",
  );
  await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
    "compiler unavailable",
  );
  expect(loads).toBe(2);
  expect(contract.runtimes()).toBe(1);
  expect(contract.freed()).toBe(2);
});

test("independent components do not load the JavaScript runtime", async () => {
  const contract = contracts();
  let loads = 0;
  const backend = new BrowserExecution({}, contract.module, unavailable(), async () => {
    loads++;
    throw new Error("Unexpected runtime load");
  });
  await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
    "compiler unavailable",
  );
  expect(loads).toBe(0);
  expect(contract.runtimes()).toBe(0);
  expect(contract.freed()).toBe(1);
});

test("a mismatched runtime is refused and released before providers are opened", async () => {
  const contract = contracts([], true);
  let opened = 0;
  const backend = new BrowserExecution(
    {
      providers: {
        filesystem: {
          data: {
            open() {
              opened++;
              throw new Error("Unexpected provider");
            },
          },
        },
      },
    },
    contract.module,
    unavailable(),
    async () => Uint8Array.of(0),
  );
  await expect(
    backend.materialize({
      artifact: new Uint8Array(),
      grants: { filesystem: [{ provider: "data", mount: "/data", writable: true }] },
    }),
  ).rejects.toThrow("Runtime identity differs");
  expect(opened).toBe(0);
  expect(contract.runtimes()).toBe(0);
  expect(contract.freed()).toBe(1);
});

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

test("one browser backend retains its verifier policy identity across different components", async () => {
  const requests: import("../src/interpreter/component/compiler.ts").CompileRequest[] = [];
  class RecordingCompiler extends TrustedCompiler {
    override async compile(
      request: import("../src/interpreter/component/compiler.ts").CompileRequest,
    ): Promise<never> {
      requests.push(request);
      throw new Error("compile recorded");
    }
  }
  const contract = contracts();
  const backend = new BrowserExecution({}, contract.module, new RecordingCompiler());
  for (let index = 0; index < 2; index++)
    await expect(backend.materialize({ artifact: Uint8Array.of(index) })).rejects.toThrow(
      "compile recorded",
    );
  expect(requests).toHaveLength(2);
  expect(requests[0]!.instrumentCore).toBe(requests[1]!.instrumentCore);
});

test("compiled byte storage is released before instance setup while contract cleanup remains exact", async () => {
  const contract = contracts();
  class RefusingCompiler extends TrustedCompiler {
    override async compile() {
      return {
        instantiate: async () => {
          expect(contract.released()).toBe(1);
          expect(contract.freed()).toBe(0);
          throw new Error("instance refused");
        },
      };
    }
  }
  const backend = new BrowserExecution({}, contract.module, new RefusingCompiler());
  await expect(backend.materialize({ artifact: new Uint8Array() })).rejects.toThrow(
    "instance refused",
  );
  expect(contract.released()).toBe(1);
  expect(contract.freed()).toBe(1);
});
