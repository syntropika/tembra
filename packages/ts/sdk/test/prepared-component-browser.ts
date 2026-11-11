// Chrome qualification of compiled Orbs through application-owned prepared adapters.
//
// `scripts/test-wasm-fixtures.ts` builds every compiled fixture with `browser: true`; the
// qualification server serves each generated `browser/` directory under `/adapters/<name>/`
// and maps the adapters' bare `@syntropika/tembra-sdk/browser` import to the page's own
// `/sdk.js`, so adapters and application share one SDK instance and its prepared-component
// brand. Every scenario loads real artifacts through the normal host path and executes the
// real JCO glue and instrumented cores; nothing here mocks the generated code.
import {
  createHost,
  fetchHttpProvider,
  HttpHandlerError,
  memoryFilesystem,
  type IncomingHttpRequest,
  type PreparedComponent,
} from "../src/browser.ts";
import type { BrowserHostOptions } from "../src/browser.ts";
import type { HttpHandlerResponse } from "../src/interpreter/http-ingress.ts";
import { verifyKeyedCompiledScenario } from "./keyed-compiled-scenario.ts";
import {
  verifyBrowserResourceOwnership,
  verifyBrowserResourceRefusal,
  verifyBrowserResourceReload,
} from "./component-resources-browser.ts";
import {
  verifyBrowserByteStreams,
  verifyBrowserByteStreamTraps,
  verifyBrowserByteStreamCancellation,
} from "./component-streams-browser.ts";
import {
  verifyCompiledSocketOperations,
  verifyCompiledSocketRefusals,
  verifyCompiledSocketRetention,
  verifyCompiledSocketCancellation,
  verifyCompiledSocketOptions,
} from "./component-sockets-browser.ts";
import {
  verifyBrowserSystemValues,
  verifyBrowserSystemStreams,
  verifyBrowserSystemIsolation,
  verifyBrowserSystemCancellation,
  verifyBrowserSystemOutputSerialization,
  verifyBrowserSystemStdioCancellation,
} from "./component-system-browser.ts";
import {
  verifyCompiledFilesystemMemory,
  verifyCompiledFilesystemOpfs,
  verifyCompiledFilesystemLifetime,
} from "./component-filesystem-browser.ts";
import {
  verifyCompiledHttpValues,
  verifyCompiledHttpDuplex,
  verifyCompiledHttpFailures,
  verifyCompiledHttpFetchLimits,
  verifyCompiledHttpLifetime,
  verifyCompiledHttpCancellation,
} from "./component-http-browser.ts";

type Host = Awaited<ReturnType<typeof createHost>>;
// Service references are dynamically typed WIT proxies.
type Reference = { release(): void; [method: string]: any };

const ARITHMETIC = "example:integrated/arithmetic@1.0.0";
const CALCULATION = "calculator::example:integrated/calculation@1.0.0";
const PIPELINE = "pipeline::example:pipeline/pipeline@1.0.0";
const PROBE = "probe::example:probe/probe@1.0.0";
const FETCHER = "http-client::example:fetch/fetcher@1.0.0";
const HOOKS = ["guard", "audit", "choice"].map((hook) => `example:hooks/${hook}@1.0.0`);
const PAGE = 65536;
const decoder = new TextDecoder();
const encoder = new TextEncoder();

/** Fetch one built qualification artifact from the test server. */
async function artifact(name: string): Promise<Uint8Array> {
  const response = await fetch(`/${name}.wasm`);
  if (!response.ok) throw new Error(`Missing fixture ${name}`);
  return new Uint8Array(await response.arrayBuffer());
}

/** Import one generated adapter module, as an application does with its build output. */
async function adapter(name: string): Promise<PreparedComponent> {
  const url = `/adapters/${name}/index.js`;
  const module = (await import(url)) as { default: PreparedComponent };
  return module.default;
}

function same(actual: unknown, expected: unknown, what: string): void {
  const [a, e] = [JSON.stringify(actual), JSON.stringify(expected)];
  if (a !== e) throw new Error(`${what}: expected ${e}, received ${a}`);
}

/** The rejection message of an operation that must fail. */
async function refusal(operation: Promise<unknown> | (() => unknown), what: string) {
  try {
    await (typeof operation === "function" ? operation() : operation);
  } catch (error) {
    return error instanceof Error ? `${error.name}: ${error.message}` : String(error);
  }
  throw new Error(`${what} succeeded`);
}

function includes(message: string, part: string, what: string): void {
  if (!message.includes(part)) throw new Error(`${what}: "${message}" lacks "${part}"`);
}

/** Close a host and require that it released every value it handed out. */
async function closed(host: Host): Promise<void> {
  await host.close();
  same(host.diagnostics().values, 0, "values after close");
}

/** Run `body` on a fresh host and always close it. */
async function using(options: BrowserHostOptions, body: (host: Host) => Promise<void>) {
  const host = await createHost(options);
  let failure: unknown;
  try {
    await body(host);
  } catch (error) {
    failure = error;
  }
  try {
    await closed(host);
  } catch (error) {
    if (!failure) failure = error;
  }
  if (failure) throw failure;
}

/** The probe's `spin` checksum, computed independently. */
function spin(rounds: number): number {
  let checksum = 0;
  for (let round = 0; round < rounds; round++) checksum = (Math.imul(checksum, 31) + round) >>> 0;
  return checksum;
}

/** The probe imports `wasi:filesystem`, so every probe installation needs a granted mount. */
const PROBE_GRANTS = {
  filesystem: [{ provider: "memory", mount: "/data", writable: false }],
};

/** Host options registering the probe adapter and the memory filesystem its grant names. */
function probeHost(
  adapters: Adapters,
  base: object,
  limits?: NonNullable<BrowserHostOptions["isolated"]>["limits"],
  components: PreparedComponent[] = [adapters.probe],
): BrowserHostOptions {
  return {
    isolated: {
      ...base,
      components,
      providers: { filesystem: { memory: memoryFilesystem({ "input.txt": "seeded" }) } },
      ...(limits ? { limits } : {}),
    },
  };
}

interface Adapters {
  arithmetic: PreparedComponent;
  pipeline: PreparedComponent;
  probe: PreparedComponent;
  hooks: PreparedComponent;
  client: PreparedComponent;
  handler: PreparedComponent;
  keyed: PreparedComponent;
  resourceProvider: PreparedComponent;
  resourceConsumer: PreparedComponent;
  streamProducer: PreparedComponent;
  streamConsumer: PreparedComponent;
  sockets: PreparedComponent;
  system: PreparedComponent;
  filesystem: PreparedComponent;
}

/** Qualify every prepared-adapter scenario; returns the passed scenario names. */
export async function verifyPreparedComponents(engine?: URL): Promise<string[]> {
  if (
    typeof (WebAssembly as unknown as Record<string, unknown>).Suspending !== "function" ||
    typeof (WebAssembly as unknown as Record<string, unknown>).promising !== "function"
  )
    throw new Error("This Chrome lacks JSPI; compiled browser qualification cannot run");
  const adapters: Adapters = {
    arithmetic: await adapter("arithmetic"),
    pipeline: await adapter("pipeline"),
    probe: await adapter("probe"),
    hooks: await adapter("hook-compiled"),
    client: await adapter("http-client"),
    handler: await adapter("http-handler"),
    keyed: await adapter("compiled-keyed-router"),
    resourceProvider: await adapter("resource-provider"),
    resourceConsumer: await adapter("resource-consumer"),
    streamProducer: await adapter("stream-producer"),
    streamConsumer: await adapter("stream-consumer"),
    sockets: await adapter("compiled-sockets"),
    system: await adapter("compiled-system"),
    filesystem: await adapter("compiled-filesystem"),
  };
  const base = engine ? { engine } : {};
  const scenarios: [string, () => Promise<void>][] = [
    ["adapter identity and SDK brand", () => brand(adapters)],
    ["registration refuses copies and look-alikes", () => lookalikes(adapters, base)],
    ["unregistered compiled Orb refused", () => unregistered(adapters, base)],
    ["mutated artifact refused by actual digest", () => mutated(adapters, base)],
    ["source calls compiled provider (host.load)", () => sourceToCompiled(adapters, base)],
    ["compiled calls source through JSPI trampolines", () => compiledToSource(adapters, base)],
    ["compiled hook contributions", () => contributions(adapters, base)],
    ["compiled reference views select keyed objects", () => keyedViews(adapters, base)],
    [
      "compiled resource ownership and borrowed methods",
      () =>
        verifyBrowserResourceOwnership(base, [
          adapters.resourceProvider,
          adapters.resourceConsumer,
        ]),
    ],
    [
      "compiled resource data-only refusal",
      () =>
        verifyBrowserResourceRefusal(base, [adapters.resourceProvider, adapters.resourceConsumer]),
    ],
    [
      "compiled resource identity across reload",
      () =>
        verifyBrowserResourceReload(base, [adapters.resourceProvider, adapters.resourceConsumer]),
    ],
    [
      "compiled byte streams and early drop",
      () => verifyBrowserByteStreams(base, [adapters.streamProducer, adapters.streamConsumer]),
    ],
    [
      "compiled byte streams report late producer traps",
      () => verifyBrowserByteStreamTraps(base, [adapters.streamProducer, adapters.streamConsumer]),
    ],
    [
      "compiled byte stream cancellation",
      () =>
        verifyBrowserByteStreamCancellation(base, [
          adapters.streamProducer,
          adapters.streamConsumer,
        ]),
    ],
    ["independent installs and reload", () => independence(adapters, base)],
    ["initial page budget", () => pages(adapters, base)],
    ["fuel exhaustion and trap quarantine", () => quarantine(adapters, base)],
    ["WASI filesystem grants", () => filesystem(adapters, base)],
    [
      "compiled WASI filesystem memory operations",
      () => verifyCompiledFilesystemMemory(base, [adapters.filesystem]),
    ],
    [
      "compiled WASI filesystem OPFS operations",
      () => verifyCompiledFilesystemOpfs(base, [adapters.filesystem]),
    ],
    [
      "compiled WASI filesystem lifetime and cancellation",
      () => verifyCompiledFilesystemLifetime(base, [adapters.filesystem]),
    ],
    [
      "compiled WASI clocks and random values",
      () => verifyBrowserSystemValues(base, [adapters.system]),
    ],
    ["compiled WASI CLI streams", () => verifyBrowserSystemStreams(base, [adapters.system])],
    [
      "compiled WASI system grants and isolation",
      () => verifyBrowserSystemIsolation(base, [adapters.system]),
    ],
    [
      "compiled WASI clock cancellation",
      () => verifyBrowserSystemCancellation(base, [adapters.system]),
    ],
    [
      "compiled WASI output serialization",
      () => verifyBrowserSystemOutputSerialization(base, [adapters.system]),
    ],
    [
      "compiled WASI stdio cancellation",
      () => verifyBrowserSystemStdioCancellation(base, [adapters.system]),
    ],
    [
      "compiled WASI socket operations",
      () => verifyCompiledSocketOperations(base, [adapters.sockets]),
    ],
    [
      "compiled WASI socket grants and refusals",
      () => verifyCompiledSocketRefusals(base, [adapters.sockets]),
    ],
    [
      "compiled WASI socket retention",
      () => verifyCompiledSocketRetention(base, [adapters.sockets]),
    ],
    [
      "compiled WASI socket cancellation",
      () => verifyCompiledSocketCancellation(base, [adapters.sockets]),
    ],
    [
      "compiled WASI socket options and addresses",
      () => verifyCompiledSocketOptions(base, [adapters.sockets]),
    ],
    ["WASI HTTP client and streamed events", () => httpClient(adapters, base)],
    ["compiled WASI HTTP resource values", () => verifyCompiledHttpValues(base, [adapters.client])],
    [
      "compiled WASI HTTP duplex and trailers",
      () => verifyCompiledHttpDuplex(base, [adapters.client]),
    ],
    ["compiled WASI HTTP failures", () => verifyCompiledHttpFailures(base, [adapters.client])],
    [
      "compiled WASI HTTP Fetch limits",
      () => verifyCompiledHttpFetchLimits(base, [adapters.client]),
    ],
    ["compiled WASI HTTP lifetime", () => verifyCompiledHttpLifetime(base, [adapters.client])],
    [
      "compiled WASI HTTP cancellation",
      () => verifyCompiledHttpCancellation(base, [adapters.client]),
    ],
    ["incoming HTTP handler", () => ingress(adapters, base)],
  ];
  const passed: string[] = [];
  const failed: string[] = [];
  for (const [name, scenario] of scenarios) {
    try {
      await scenario();
      passed.push(name);
    } catch (error) {
      failed.push(`${name}: ${error instanceof Error ? (error.stack ?? error.message) : error}`);
    }
  }
  if (failed.length)
    throw new Error(
      `${failed.length} of ${scenarios.length} prepared-component scenarios failed:\n` +
        failed.join("\n"),
    );
  return passed;
}

/** Generated modules carry the verified identity and exact digest; they are frozen. */
async function brand(adapters: Adapters): Promise<void> {
  const expected: [keyof Adapters, string][] = [
    ["arithmetic", "arithmetic"],
    ["pipeline", "pipeline"],
    ["probe", "probe"],
    ["hooks", "hook-compiled"],
    ["client", "http-client"],
    ["handler", "http-handler"],
    ["keyed", "compiled-keyed-router"],
    ["resourceProvider", "resource-provider"],
    ["resourceConsumer", "resource-consumer"],
    ["streamProducer", "stream-producer"],
    ["streamConsumer", "stream-consumer"],
    ["sockets", "compiled-sockets"],
    ["system", "compiled-system"],
    ["filesystem", "compiled-filesystem"],
  ];
  for (const [key, name] of expected) {
    const component = adapters[key];
    same(component.orb, { name, apiVersion: 1 }, `${name} identity`);
    const digest = new Uint8Array(
      await crypto.subtle.digest("SHA-256", (await artifact(name)) as BufferSource),
    );
    const hex = [...digest].map((byte) => byte.toString(16).padStart(2, "0")).join("");
    same(component.artifact, hex, `${name} adapter digest equals the served artifact`);
    same(Object.isFrozen(component), true, `${name} adapter is frozen`);
  }
  // The same module imported again is the same registered value.
  same((await adapter("probe")) === adapters.probe, true, "module singleton");
}

/** Only values created by this SDK's `preparedComponent` are accepted. */
async function lookalikes(adapters: Adapters, base: object): Promise<void> {
  const copies = [
    { ...adapters.probe },
    Object.freeze({ orb: adapters.probe.orb, artifact: adapters.probe.artifact }),
    Object.create(adapters.probe),
  ];
  for (const copy of copies)
    includes(
      await refusal(
        createHost({ isolated: { ...base, components: [copy as PreparedComponent] } }),
        "look-alike registration",
      ),
      "accepts only modules generated by Tembra tooling",
      "look-alike refusal",
    );
  includes(
    await refusal(
      createHost({ isolated: { ...base, components: adapters.probe as never } }),
      "non-list registration",
    ),
    "must list prepared components",
    "non-list refusal",
  );
  // Registering one adapter twice is harmless.
  await using(probeHost(adapters, base, undefined, [adapters.probe, adapters.probe]), async (h) => {
    await h.install({ artifact: await artifact("probe"), grants: PROBE_GRANTS });
  });
}

/** A compiled Orb without a registered adapter never executes. */
async function unregistered(adapters: Adapters, base: object): Promise<void> {
  const probe = await artifact("probe");
  await using({ isolated: base }, async (host) => {
    const message = await refusal(
      host.install({ artifact: probe, grants: PROBE_GRANTS }),
      "unregistered install",
    );
    includes(message, "browser: true", "no-adapter refusal names the build option");
    includes(message, "isolated: { components", "no-adapter refusal names the registration");
    // Source Orbs still run on the same host.
    await host.install({ artifact: await artifact("arithmetic-source") });
    const reference = (await host.get(`arithmetic::${ARITHMETIC}`, "main")) as Reference;
    same(await reference.double(4), 8, "source Orb beside the refusal");
    reference.release();
  });
  // Other adapters registered: the probe still matches none of them.
  await using(
    { isolated: { ...base, components: [adapters.arithmetic, adapters.hooks] } },
    async (host) => {
      includes(
        await refusal(
          host.install({ artifact: probe, grants: PROBE_GRANTS }),
          "install with other adapters",
        ),
        "matches no registered prepared adapter",
        "foreign-adapter refusal",
      );
    },
  );
}

/** One changed byte deep inside the component changes the digest the verifier computes. */
async function mutated(adapters: Adapters, base: object): Promise<void> {
  const original = await artifact("probe");
  const changed = original.slice();
  const offset = Math.floor(changed.length / 2);
  changed[offset] = changed[offset]! ^ 0x01;
  await using(probeHost(adapters, base), async (host) => {
    includes(
      await refusal(
        host.install({ artifact: changed, grants: PROBE_GRANTS }),
        "mutated artifact install",
      ),
      "matches no registered prepared adapter",
      "mutated-artifact refusal",
    );
    // The refusal leaves the host usable for the exact artifact.
    await host.install({ artifact: original, grants: PROBE_GRANTS });
    const probe = (await host.get(PROBE, "main")) as Reference;
    same(await probe.spin(100), spin(100), "exact artifact after the refusal");
    probe.release();
  });
}

/** Source calculator → compiled arithmetic provider, loaded through `host.load`. */
async function sourceToCompiled(adapters: Adapters, base: object): Promise<void> {
  const requires = { [ARITHMETIC]: { orb: "arithmetic", apiVersion: 1 } };
  await using({ isolated: { ...base, components: [adapters.arithmetic] } }, async (host) => {
    await host.load("/arithmetic.wasm");
    await host.load("/calculator.wasm", { requires });
    const arithmetic = (await host.get(`arithmetic::${ARITHMETIC}`, "main")) as Reference;
    const calculation = (await host.get(CALCULATION, "main")) as Reference;
    try {
      same(await arithmetic.double(21), 42, "direct compiled call");
      same(await arithmetic.double(-7), -14, "negative s32");
      same(await calculation.sum({ values: [2, 4, 6] }), 24, "source sum via compiled double");
      same(await calculation.fanout(), 24, "three concurrent compiled calls from source");
      same(
        await Promise.all([1, 2, 3].map((value) => arithmetic.double(value))),
        [2, 4, 6],
        "concurrent host calls are serialized by the revision gate",
      );
    } finally {
      arithmetic.release();
      calculation.release();
    }
  });
}

/** Compiled pipeline → two waiting source providers, via sync and P3 async imports. */
async function compiledToSource(adapters: Adapters, base: object): Promise<void> {
  const requires = {
    "example:pipeline/arithmetic@1.0.0": { orb: "pipeline-arithmetic", apiVersion: 1 },
    "example:pipeline/delayed@1.0.0": { orb: "pipeline-delayed", apiVersion: 1 },
  };
  await using({ isolated: { ...base, components: [adapters.pipeline] } }, async (host) => {
    // A missing dependency is refused before instantiation.
    await refusal(host.load("/pipeline.wasm", { requires }), "pipeline without providers");
    await host.install([
      { artifact: await artifact("pipeline-arithmetic") },
      { artifact: await artifact("pipeline-delayed") },
    ]);
    await host.load("/pipeline.wasm", { requires });
    const pipeline = (await host.get(PIPELINE, "main")) as Reference;
    try {
      const started = performance.now();
      same(await pipeline.quadruple(6), 24, "sync import twice (conditional suspending)");
      same(await pipeline.octuple(3), 24, "P3 async import three times");
      // Five sequential dependency calls each waited 20 ms inside a source provider.
      const elapsed = performance.now() - started;
      if (elapsed < 100) throw new Error(`Dependency waits were not observed (${elapsed} ms)`);
      same(
        await Promise.all([pipeline.quadruple(1), pipeline.octuple(1), pipeline.quadruple(-2)]),
        [4, 8, -8],
        "concurrent compiled callbacks queue",
      );
      // Caller context flows through the compiled consumer into its providers.
      same(
        await host.root((context) => pipeline.quadruple(5, context)),
        20,
        "call under an explicit root",
      );
    } finally {
      pipeline.release();
    }
  });
}

/** The compiled router selects keyed source counters through its checked reference view. */
async function keyedViews(adapters: Adapters, base: object): Promise<void> {
  await using({ isolated: { ...base, components: [adapters.keyed] } }, async (host) => {
    await verifyKeyedCompiledScenario(host, artifact);
  });
}

/** The compiled contributor joins waterfall, series and bail hooks before source participants. */
async function contributions(adapters: Adapters, base: object): Promise<void> {
  const requires = Object.fromEntries(HOOKS.map((hook) => [hook, { orb: "tools", apiVersion: 1 }]));
  await using({ isolated: { ...base, components: [adapters.hooks] } }, async (host) => {
    await host.install([
      { artifact: await artifact("hook-provider") },
      { artifact: await artifact("hook-extension"), requires },
    ]);
    await host.load("/hook-compiled.wasm", { requires });
    const runner = (await host.get("tools::example:hooks/runner@1.0.0", "main")) as Reference;
    const source = (await host.get("extension::example:hooks/reporter@1.0.0", "main")) as Reference;
    const compiled = (await host.get(
      "hook-compiled::example:hooks/reporter@1.0.0",
      "main",
    )) as Reference;
    try {
      // Waterfall in plan order: compiled (-7) adds 20, then source (0) adds 2 and blocks.
      same(await runner.run({ value: 3, blocked: false }), { value: 25, blocked: true }, "guard");
      for (const input of [5, 7]) same(await runner.audit(input), null, "audit");
      same(await runner.choose(0), null, "choice undecided");
      same(await runner.choose(1), 42, "choice decided by the compiled contributor");
      same(await runner.choose(2), 84, "choice decided by the source contributor");
      same(await source.read(), { audit: 12, choices: 2 }, "source contributor counts");
      same(await compiled.read(), { audit: 12, choices: 3 }, "compiled contributor counts");
    } finally {
      for (const reference of [runner, source, compiled]) reference.release();
    }
  });
}

/** One adapter serves independent instances: separate hosts, and a reload's new instance. */
async function independence(adapters: Adapters, base: object): Promise<void> {
  const requires = Object.fromEntries(HOOKS.map((hook) => [hook, { orb: "tools", apiVersion: 1 }]));
  const options = { isolated: { ...base, components: [adapters.hooks] } };
  const compiledArtifact = await artifact("hook-compiled");
  const install = async (host: Host) => {
    await host.install([
      { artifact: await artifact("hook-provider") },
      { artifact: compiledArtifact, requires },
    ]);
  };
  const counts = async (host: Host) => {
    const reporter = (await host.get(
      "hook-compiled::example:hooks/reporter@1.0.0",
      "main",
    )) as Reference;
    try {
      return await reporter.read();
    } finally {
      reporter.release();
    }
  };
  const first = await createHost(options);
  const second = await createHost(options);
  try {
    await install(first);
    await install(second);
    const runner = (await first.get("tools::example:hooks/runner@1.0.0", "main")) as Reference;
    await runner.audit(9);
    await runner.audit(4);
    runner.release();
    same(await counts(first), { audit: 13, choices: 0 }, "first instance state");
    same(await counts(second), { audit: 0, choices: 0 }, "second instance memory is its own");
    await closed(first);
    // Closing one installation leaves the other's instance running.
    const other = (await second.get("tools::example:hooks/runner@1.0.0", "main")) as Reference;
    await other.audit(2);
    other.release();
    same(await counts(second), { audit: 2, choices: 0 }, "second instance after first closed");
    // A reload instantiates the same adapter again with fresh linear memory.
    await second.reload([{ artifact: compiledArtifact, requires }]);
    same(await counts(second), { audit: 0, choices: 0 }, "reloaded instance memory is fresh");
    const reloaded = (await second.get("tools::example:hooks/runner@1.0.0", "main")) as Reference;
    await reloaded.audit(6);
    reloaded.release();
    same(await counts(second), { audit: 6, choices: 0 }, "reloaded instance participates");
  } finally {
    await first.close();
    await closed(second);
  }
}

/** Initial pages of every instantiation are debited from `limits.memoryBytes`. */
async function pages(adapters: Adapters, base: object): Promise<void> {
  const probe = await artifact("probe");
  // The probe's cores start with 17 pages in total.
  for (const pagesAllowed of [1, 16]) {
    await using(probeHost(adapters, base, { memoryBytes: pagesAllowed * PAGE }), async (host) => {
      includes(
        await refusal(
          host.install({ artifact: probe, grants: PROBE_GRANTS }),
          `install with ${pagesAllowed} pages`,
        ),
        "memory limit exceeded",
        "initial page budget refusal",
      );
      // A later installation on the same host fails the same way: no partial debit leaked.
      includes(
        await refusal(host.install({ artifact: probe, grants: PROBE_GRANTS }), "repeated install"),
        "memory limit exceeded",
        "repeated page budget refusal",
      );
    });
  }
  await using(probeHost(adapters, base, { memoryBytes: 17 * PAGE }), async (host) => {
    await host.install({ artifact: probe, grants: PROBE_GRANTS });
    const reference = (await host.get(PROBE, "main")) as Reference;
    same(await reference.spin(10), spin(10), "exact initial page budget");
    reference.release();
  });
  // A budget below one page is refused as a configuration error.
  await using(probeHost(adapters, base, { memoryBytes: PAGE - 1 }), async (host) => {
    includes(
      await refusal(host.install({ artifact: probe, grants: PROBE_GRANTS }), "sub-page budget"),
      "limits.memoryBytes",
      "sub-page budget refusal",
    );
  });
}

/** Fuel exhaustion and a Wasm trap quarantine only the affected instance. */
async function quarantine(adapters: Adapters, base: object): Promise<void> {
  const probe = await artifact("probe");
  await using(probeHost(adapters, base, { fuel: 500_000 }), async (host) => {
    await host.install({ artifact: probe, grants: PROBE_GRANTS });
    const reference = (await host.get(PROBE, "main")) as Reference;
    try {
      same(await reference.spin(1000), spin(1000), "spin within fuel");
      // Fuel is refilled per callback, so repeated bounded calls keep working.
      for (let call = 0; call < 3; call++)
        same(await reference.spin(50_000), spin(50_000), "refilled fuel");
      await refusal(reference.spin(50_000_000), "spin beyond fuel");
      includes(
        await refusal(reference.spin(1), "call after exhaustion"),
        "quarantined",
        "quarantine after fuel exhaustion",
      );
    } finally {
      reference.release();
    }
  });
  await using(probeHost(adapters, base), async (host) => {
    // Two installations of one adapter on one host under different names are impossible
    // (one artifact, one Orb name), so the trap's isolation is checked against a second host.
    const bystander = await createHost(probeHost(adapters, base));
    try {
      await host.install({ artifact: probe, grants: PROBE_GRANTS });
      await bystander.install({ artifact: probe, grants: PROBE_GRANTS });
      const trapped = (await host.get(PROBE, "main")) as Reference;
      const healthy = (await bystander.get(PROBE, "main")) as Reference;
      await refusal(trapped.trap(), "trap");
      includes(await refusal(trapped.spin(1), "call after trap"), "quarantined", "trap");
      same(await healthy.spin(7), spin(7), "other instance unaffected by the trap");
      trapped.release();
      healthy.release();
    } finally {
      await closed(bystander);
    }
  });
}

/** Standard WASI filesystem through granted memory mounts, writable and read-only. */
async function filesystem(adapters: Adapters, base: object): Promise<void> {
  const memory = memoryFilesystem({ "input.txt": "seeded" });
  await using(
    {
      isolated: { ...base, components: [adapters.probe], providers: { filesystem: { memory } } },
    },
    async (host) => {
      await host.install({
        artifact: await artifact("probe"),
        grants: {
          filesystem: [
            { provider: "memory", mount: "/data", writable: true },
            { provider: "memory", mount: "/view", writable: false },
          ],
        },
      });
      const probe = (await host.get(PROBE, "main")) as Reference;
      try {
        const ok = { tag: "ok" };
        const tag = (value: { tag: string }) => ({ tag: value.tag });
        same(tag(await probe.open("/data/input.txt")), ok, "open granted file");
        same(tag(await probe["create-file"]("/data/new.txt")), ok, "create in writable mount");
        same(tag(await probe.open("/data/new.txt")), ok, "created file visible");
        same(tag(await probe["create-directory"]("/data/dir")), ok, "create directory");
        same(tag(await probe["remove-directory"]("/data/dir")), ok, "remove directory");
        same(tag(await probe.open("/view/input.txt")), ok, "read-only mount reads");
        const readOnly = await probe["create-file"]("/view/other.txt");
        same(readOnly.tag, "err", "read-only mount refuses creation");
        const outside = await probe.open("/elsewhere/input.txt");
        same(
          outside,
          { tag: "err", val: "/elsewhere/input.txt is outside every preopen" },
          "ungranted path",
        );
        same((await probe.open("/data/missing.txt")).tag, "err", "missing file");
      } finally {
        probe.release();
      }
    },
  );
  // An imported filesystem without any grant is refused at installation, before instantiation.
  await using(probeHost(adapters, base), async (host) => {
    includes(
      await refusal(host.install({ artifact: await artifact("probe") }), "ungranted probe"),
      "no provider has been granted to this Orb",
      "filesystem import without a grant",
    );
  });
}

/** Outgoing `wasi:http` requests through an exact-origin grant, including a held SSE stream. */
async function httpClient(adapters: Adapters, base: object): Promise<void> {
  let requests = 0;
  const page = fetchHttpProvider((input, init) => {
    requests++;
    return globalThis.fetch(input, init);
  });
  await using(
    { isolated: { ...base, components: [adapters.client], providers: { http: { page } } } },
    async (host) => {
      await host.install({
        artifact: await artifact("http-client"),
        grants: { http: [{ provider: "page", origins: [location.origin] }] },
      });
      const fetcher = (await host.get(FETCHER, "main")) as Reference;
      const scheme = location.protocol.slice(0, -1);
      try {
        same(
          await fetcher.fetch(scheme, location.host, "/wasi/values"),
          { tag: "ok", val: '200 {"values":[2,4,6]}' },
          "granted GET",
        );
        // The SSE response never ends: the first event arrives before EOF and the component
        // drops the body afterwards.
        same(
          await fetcher.first(scheme, location.host, "/wasi/events"),
          { tag: "ok", val: "data: browser fetch\n\n" },
          "first streamed event",
        );
        same(requests, 2, "requests reached the provider");
        same(
          (await fetcher.fetch("https", "ungranted.example.test", "/values")).tag,
          "err",
          "ungranted origin",
        );
        same(requests, 2, "the ungranted request never reached the provider");
      } finally {
        fetcher.release();
      }
    },
  );
  // An imported wasi:http without any grant is refused at installation, before instantiation.
  await using(
    { isolated: { ...base, components: [adapters.client], providers: { http: { page } } } },
    async (host) => {
      includes(
        await refusal(
          host.install({ artifact: await artifact("http-client") }),
          "ungranted client",
        ),
        "no provider has been granted to this Orb",
        "wasi:http import without a grant",
      );
    },
  );
  same(requests, 2, "no request without a grant");
}

/** A request body the test feeds chunk by chunk. */
function body() {
  const queue: Uint8Array[] = [];
  let ended = false;
  let wake: (() => void) | undefined;
  return {
    stream: {
      [Symbol.asyncIterator]: () => ({
        async next(): Promise<IteratorResult<Uint8Array>> {
          while (!queue.length && !ended)
            await new Promise<void>((resolve) => {
              wake = resolve;
            });
          const value = queue.shift();
          return value ? { done: false, value } : { done: true, value: undefined };
        },
        async return(): Promise<IteratorResult<Uint8Array>> {
          ended = true;
          wake?.();
          return { done: true, value: undefined };
        },
      }),
    } as AsyncIterable<Uint8Array>,
    push(text: string) {
      queue.push(encoder.encode(text));
      wake?.();
    },
    end() {
      ended = true;
      wake?.();
    },
  };
}

function request(path: string, stream: AsyncIterable<Uint8Array> | null): IncomingHttpRequest {
  return {
    method: "POST",
    scheme: "https",
    authority: "app.example.test",
    pathWithQuery: path,
    fields: [],
    body: stream,
  };
}

function field(response: HttpHandlerResponse, name: string): string | undefined {
  const entry = response.fields.find(([key]) => key === name);
  return entry && decoder.decode(entry[1]);
}

/** The compiled `wasi:http/handler` export served through `Host.handleHttp`. */
async function ingress(adapters: Adapters, base: object): Promise<void> {
  await using({ isolated: { ...base, components: [adapters.handler] } }, async (host) => {
    await host.load("/http-handler.wasm");
    const input = body();
    const sse = await host.handleHttp("http-handler", request("/sse", input.stream));
    same([sse.status, field(sse, "content-type")], [200, "text/event-stream"], "SSE head");
    const events = sse.body[Symbol.asyncIterator]();
    const next = async () => decoder.decode((await events.next()).value);
    same(await next(), "data: open\n\n", "first event before the request body ended");
    for (const chunk of ["alpha", "beta"]) {
      input.push(chunk);
      same(await next(), `data: ${chunk}\n\n`, "streamed request chunk echoed");
    }
    input.end();
    same(await next(), "data: end\n\n", "end event");
    same((await events.next()).done, true, "SSE end");
    same(
      (await sse.trailers)?.map(([name, value]) => [name, decoder.decode(value)]),
      [["x-events", "4"]],
      "SSE trailers",
    );
    // Physical completion: the producer observed its transmission result.
    let status: HttpHandlerResponse | undefined;
    for (let attempt = 0; attempt < 50; attempt++) {
      status = await host.handleHttp("http-handler", request("/status", null));
      const contents = status.body[Symbol.asyncIterator]();
      while (!(await contents.next()).done);
      if (field(status, "x-last-exit")?.startsWith("complete")) break;
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    const exit = field(status!, "x-last-exit") ?? "";
    if (!exit.startsWith("complete events=4 transmit=Ok"))
      throw new Error(`Producer exit was ${exit}`);
    const refused = await host
      .handleHttp("http-handler", request("/error", null))
      .catch((error: unknown) => error);
    same(refused instanceof HttpHandlerError, true, "handler error-code");
    same((refused as HttpHandlerError).code.tag, "internal-error", "error-code tag");
    const trapped = await host
      .handleHttp("http-handler", request("/trap", null))
      .catch((error: unknown) => error);
    same(
      trapped instanceof Error && !(trapped instanceof HttpHandlerError),
      true,
      "handler trap rejects",
    );
    includes(
      String(
        await host.handleHttp("http-handler", request("/status", null)).catch((error) => error),
      ),
      "quarantined",
      "handler quarantined after its trap",
    );
  });
}
