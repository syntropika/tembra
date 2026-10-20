import { afterEach, mock, spyOn, test } from "bun:test";
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { setTimeout as delay } from "node:timers/promises";

import { createBrowserBridge, createStdioHost, HostDeliveryError } from "../src/adapters.js";
import { connectDevelopmentHost } from "../src/browser.js";
import type { BuiltArtifact } from "../src/types.js";

afterEach(() => {
  mock.restore();
});

function artifact(target = "example"): BuiltArtifact {
  return {
    target,
    platform: "browser",
    artifact: {
      format: "tembra-esm-v1",
      identity: target,
      sha256: "0".repeat(64),
      source: "export default {};",
      exports: ["default"],
    },
    selectedExport: "default",
    sourcePath: "unused",
    artifactPath: "unused",
    bytes: 18,
    inputs: [],
  };
}
const signal = () => new AbortController().signal;
function uninitializedGate(): never {
  throw new Error("The controlled apply gate was not initialized");
}
async function offer(response: Response): Promise<{ id: number; targets: string[] }> {
  assert.equal(response.status, 200);
  const value: unknown = await response.json();
  assert.ok(
    value !== null && typeof value === "object" && "id" in value && typeof value.id === "number",
  );
  assert.ok("artifacts" in value && Array.isArray(value.artifacts));
  return {
    id: value.id,
    targets: value.artifacts.map((entry: unknown) => {
      assert.ok(
        entry !== null &&
          typeof entry === "object" &&
          "target" in entry &&
          typeof entry.target === "string",
      );
      return entry.target;
    }),
  };
}
async function until(predicate: () => boolean): Promise<void> {
  const deadline = Date.now() + 5000;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error("Condition did not settle");
    }
    // oxlint-disable-next-line no-await-in-loop -- Poll a live transport postcondition until the explicit readiness or cleanup evidence is visible.
    await delay(10);
  }
}

const unsafeTokens = [
  { name: "short token", token: "a".repeat(15) },
  { name: "leading space", token: ` ${"a".repeat(16)}` },
  { name: "trailing space", token: `${"a".repeat(16)} ` },
  { name: "embedded space", token: `${"a".repeat(8)} ${"b".repeat(8)}` },
  { name: "leading tab", token: `\t${"a".repeat(16)}` },
  { name: "trailing tab", token: `${"a".repeat(16)}\t` },
  ...[
    { name: "embedded tab", character: "\t" },
    { name: "NUL", character: "\0" },
    { name: "control byte", character: "\u0001" },
    { name: "vertical tab", character: "\v" },
    { name: "form feed", character: "\f" },
    { name: "carriage return", character: "\r" },
    { name: "line feed", character: "\n" },
    { name: "escape", character: "\u001B" },
    { name: "unit separator", character: "\u001F" },
    { name: "DEL", character: "\u007F" },
    { name: "non-ASCII Latin character", character: "\u00E9" },
    { name: "nonbreaking space", character: "\u00A0" },
    { name: "Unicode whitespace", character: "\u2003" },
    { name: "non-ASCII code point", character: "\u{1F511}" },
    { name: "unpaired surrogate", character: "\uD800" },
  ].map(({ name, character }) => ({ name, token: `${"a".repeat(8)}${character}${"b".repeat(8)}` })),
];

test.each(unsafeTokens)(
  "browser bridge refuses unsafe custom $name before listening",
  async ({ token }) => {
    const occupied = await createBrowserBridge();
    try {
      const port = Number(new URL(occupied.url).port);
      await assert.rejects(
        createBrowserBridge({ token, port }),
        (error: unknown) => error instanceof TypeError && /printable ASCII/u.test(error.message),
      );
    } finally {
      await occupied.close();
    }
  },
);

test("browser bridge refuses non-string custom tokens before listening", async () => {
  const occupied = await createBrowserBridge();
  try {
    const port = Number(new URL(occupied.url).port);
    await assert.rejects(
      async () => {
        await Reflect.apply(createBrowserBridge, undefined, [
          { token: 1_234_567_890_123_456, port },
        ]);
      },
      (error: unknown) => error instanceof TypeError && /printable ASCII/u.test(error.message),
    );
  } finally {
    await occupied.close();
  }
});

test("custom printable ASCII tokens survive authorization and acknowledgement unchanged", async () => {
  const token = Array.from({ length: 94 }, (_value, index) =>
    String.fromCodePoint(index + 33),
  ).join("");
  const bridge = await createBrowserBridge({ token });
  let connection: ReturnType<typeof connectDevelopmentHost> | undefined;
  try {
    assert.equal(bridge.token, token);
    const unauthorized = await fetch(`${bridge.url}/updates?client=unauthorized`, {
      headers: { Authorization: `Bearer wrong-${token}` },
    });
    assert.equal(unauthorized.status, 401);
    let applied = 0;
    connection = connectDevelopmentHost({
      url: bridge.url,
      token,
      async apply() {
        applied += 1;
      },
    });
    await connection.ready;
    const result = await bridge.apply([artifact()], signal());
    assert.equal(result.status, "applied");
    assert.equal(applied, 1);
  } finally {
    await connection?.close();
    await bridge.close();
  }
});

test("browser bridge requires capability and acknowledges the actual apply result", async () => {
  const bridge = await createBrowserBridge();
  let connection: ReturnType<typeof connectDevelopmentHost> | undefined;
  try {
    const unauthorized = await fetch(`${bridge.url}/updates?client=test`);
    const disconnected = await bridge.apply([artifact()], signal());
    assert.equal(unauthorized.status, 401);
    assert.equal(disconnected.status, "pending");
    let applied = 0;
    const targets: string[][] = [];
    connection = connectDevelopmentHost({
      url: bridge.url,
      token: bridge.token,
      async apply(artifacts) {
        targets.push(artifacts.map((value) => value.target));
        applied += 1;
      },
    });
    await connection.ready;
    await until(() => applied === 1);
    const result = await bridge.apply([artifact("second")], signal());
    assert.equal(result.status, "applied");
    assert.equal(applied, 2);
    assert.deepEqual(targets, [["example"], ["second"]]);
  } finally {
    await connection?.close();
    await bridge.close();
  }
});

test("two browser clients acknowledge independently without replaying unchanged artifacts", async () => {
  const bridge = await createBrowserBridge();
  const headers = { Authorization: `Bearer ${bridge.token}`, "Content-Type": "application/json" };
  const poll = async (client: string) =>
    await fetch(`${bridge.url}/updates?client=${client}`, { headers });
  const acknowledge = async (client: string, id: number) => {
    const response = await fetch(`${bridge.url}/ack?client=${client}`, {
      headers,
      method: "POST",
      body: JSON.stringify({ id, ok: true }),
    });
    assert.equal(response.status, 204);
  };
  try {
    const readiness = await Promise.all([poll("left"), poll("right")]);
    assert.deepEqual(
      readiness.map((response) => response.status),
      [204, 204],
    );
    let applied = false;
    const delivery = bridge.apply([artifact("one")], signal()).then((result) => {
      applied = true;
      return result;
    });
    const [left, right] = await Promise.all([poll("left").then(offer), poll("right").then(offer)]);
    assert.deepEqual(left.targets, ["one"]);
    assert.deepEqual(right.targets, ["one"]);
    assert.equal(left.id, right.id);
    assert.deepEqual(
      await offer(await poll("left")),
      left,
      "an unacknowledged offer survives a lost poll response",
    );
    await acknowledge("left", left.id);
    assert.equal(applied, false, "one browser's acknowledgement cannot acknowledge the other");
    await acknowledge("right", right.id);
    assert.deepEqual(await delivery, { status: "applied" });
    const repeated = await Promise.all(
      Array.from(
        { length: 20 },
        async (_value, index) => await poll(index % 2 === 0 ? "left" : "right"),
      ),
    );
    assert.ok(repeated.every((response) => response.status === 204));

    const changed = bridge.apply([artifact("two")], signal());
    const updates = await Promise.all([poll("left").then(offer), poll("right").then(offer)]);
    assert.deepEqual(
      updates.map((value) => value.targets),
      [["two"], ["two"]],
    );
    await Promise.all(
      updates.map(async (value, index) => {
        await acknowledge(index === 0 ? "left" : "right", value.id);
      }),
    );
    const updated = await changed;
    assert.equal(updated.status, "applied");
    const fresh = await offer(await poll("fresh"));
    assert.deepEqual(fresh.targets, ["one", "two"]);
    await acknowledge("fresh", fresh.id);
    const unchanged = await poll("fresh");
    assert.equal(unchanged.status, 204);
  } finally {
    await bridge.close();
  }
});

test("two actual polling helpers each apply a changed target once", async () => {
  const bridge = await createBrowserBridge();
  const received: string[][][] = [[], []];
  const connections = received.map((updates) =>
    connectDevelopmentHost({
      url: bridge.url,
      token: bridge.token,
      async apply(artifacts) {
        updates.push(artifacts.map((value) => value.target));
      },
    }),
  );
  try {
    await Promise.all(
      connections.map(async (connection) => {
        await connection.ready;
      }),
    );
    const initial = await bridge.apply([artifact("one")], signal());
    assert.equal(initial.status, "applied");
    const update = await bridge.apply([artifact("two")], signal());
    assert.equal(update.status, "applied");
    assert.deepEqual(received, [
      [["one"], ["two"]],
      [["one"], ["two"]],
    ]);
    assert.deepEqual(await bridge.apply([], signal()), { status: "applied" });
  } finally {
    await Promise.all(
      connections.map(async (connection) => {
        await connection.close();
      }),
    );
    await bridge.close();
  }
});

test("a lost acknowledgement response retries without applying the browser update again", async () => {
  const bridge = await createBrowserBridge();
  const nativeFetch = globalThis.fetch;
  let acknowledgements = 0;
  let applications = 0;
  spyOn(globalThis, "fetch").mockImplementation(
    Object.assign(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const response = await nativeFetch(input, init);
        let address: string;
        if (typeof input === "string") {
          address = input;
        } else if (input instanceof URL) {
          address = input.href;
        } else {
          address = input.url;
        }
        if (address.includes("/ack?")) {
          acknowledgements += 1;
          if (acknowledgements === 1) {
            throw new TypeError(
              "The acknowledgement response was lost after the server received it",
            );
          }
        }
        return response;
      },
      { preconnect: nativeFetch.preconnect },
    ),
  );
  const connection = connectDevelopmentHost({
    url: bridge.url,
    token: bridge.token,
    async apply() {
      applications += 1;
    },
  });
  try {
    await connection.ready;
    const delivered = await bridge.apply([artifact()], signal());
    assert.equal(delivered.status, "applied");
    await until(() => acknowledgements >= 2);
    assert.equal(applications, 1);
  } finally {
    await connection.close();
    await bridge.close();
  }
});

test("a lost offered response retries without losing the pending browser delivery", async () => {
  const bridge = await createBrowserBridge();
  const nativeFetch = globalThis.fetch;
  let lost = false;
  let applications = 0;
  spyOn(globalThis, "fetch").mockImplementation(
    Object.assign(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        const response = await nativeFetch(input, init);
        if (response.status === 200 && !lost) {
          lost = true;
          throw new TypeError("The offered response was lost before the browser received it");
        }
        return response;
      },
      { preconnect: nativeFetch.preconnect },
    ),
  );
  const connection = connectDevelopmentHost({
    url: bridge.url,
    token: bridge.token,
    async apply() {
      applications += 1;
    },
  });
  try {
    await connection.ready;
    const delivered = await bridge.apply([artifact()], signal());
    assert.equal(delivered.status, "applied");
    assert.equal(lost, true);
    assert.equal(applications, 1);
  } finally {
    await connection.close();
    await bridge.close();
  }
});

test("browser client records are bounded and idle records expire", async () => {
  let now = 100_000;
  spyOn(Date, "now").mockImplementation(() => now);
  const bridge = await createBrowserBridge();
  const headers = { Authorization: `Bearer ${bridge.token}` };
  const poll = async (client: number) =>
    await fetch(`${bridge.url}/updates?client=${client}`, { headers });
  try {
    const active = await Promise.all(
      Array.from({ length: 128 }, async (_value, index) => await poll(index)),
    );
    assert.ok(active.every((response) => response.status === 204));
    const excess = await poll(128);
    assert.equal(excess.status, 429);
    now += 30_001;
    const admitted = await poll(128);
    assert.equal(admitted.status, 204);
  } finally {
    await bridge.close();
  }
});

test("expired acknowledged browser clients stop without replaying installed code", async () => {
  let now = 100_000;
  spyOn(Date, "now").mockImplementation(() => now);
  const bridge = await createBrowserBridge();
  const headers = { Authorization: `Bearer ${bridge.token}` };
  const nativeFetch = globalThis.fetch;
  let applications = 0;
  let polls = 0;
  let expired = false;
  let stoppedAt = 0;
  const errors: Error[] = [];
  let reportFailure: (error: Error) => void = uninitializedGate;
  // oxlint-disable-next-line promise/avoid-new -- The explicit expiry diagnostic settles this test's owned observation; the test deadline bounds a missing diagnostic.
  const refusal = new Promise<Error>((resolve) => {
    reportFailure = resolve;
  });
  const failureTimeout = setTimeout(() => {
    reportFailure(new Error("The helper did not report expiry before the test deadline"));
  }, 4500);
  spyOn(globalThis, "fetch").mockImplementation(
    Object.assign(
      async (input: RequestInfo | URL, init?: RequestInit) => {
        if (input instanceof URL && input.pathname === "/updates") {
          polls += 1;
          const cursor = input.searchParams.get("cursor");
          if (cursor !== null && cursor !== "0" && !expired) {
            // Simulate a silent interval between acknowledged delivery and the next actual HTTP poll.
            now += 30_001;
            expired = true;
          }
        }
        return await nativeFetch(input, init);
      },
      { preconnect: nativeFetch.preconnect },
    ),
  );
  let connection: ReturnType<typeof connectDevelopmentHost> | undefined;
  try {
    const malformed = await Promise.all(
      ["", "-1", "1.5", "NaN", "9007199254740992", "1&cursor=2"].map(
        async (cursor) =>
          await fetch(`${bridge.url}/updates?client=invalid&cursor=${cursor}`, { headers }),
      ),
    );
    assert.ok(malformed.every((response) => response.status === 400));
    const unknown = await fetch(`${bridge.url}/updates?client=unknown&cursor=1`, { headers });
    assert.equal(unknown.status, 409);
    connection = connectDevelopmentHost({
      url: bridge.url,
      token: bridge.token,
      async apply() {
        applications += 1;
      },
      onError(error) {
        errors.push(error);
        stoppedAt = polls;
        reportFailure(error);
      },
    });
    await connection.ready;
    const result = await bridge.apply([artifact()], signal());
    assert.equal(result.status, "applied");
    const error = await refusal;
    assert.match(error.message, /cursor.*installed.*Reconnect/u);
    await connection.close();
    assert.equal(expired, true);
    assert.equal(applications, 1, "expired connection must not spend another document evaluation");
    assert.equal(errors.length, 1);
    assert.equal(polls, stoppedAt, "the helper joins its stopped poll loop after the diagnostic");
  } finally {
    clearTimeout(failureTimeout);
    await connection?.close();
    await bridge.close();
  }
}, 5000);

test("one browser refusal waits for sibling acknowledgement and does not roll back that browser", async () => {
  const bridge = await createBrowserBridge();
  const good = artifact();
  const bad = { ...artifact(), artifact: { ...good.artifact, source: "refused by one browser" } };
  const received: string[][] = [[], []];
  let release: () => void = uninitializedGate;
  // oxlint-disable-next-line promise/avoid-new -- The test owns an explicit apply-completion gate and releases it before connection cleanup.
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let held = false;
  const connections = received.map((sources, index) =>
    connectDevelopmentHost({
      url: bridge.url,
      token: bridge.token,
      async apply(artifacts) {
        const [update] = artifacts;
        assert.ok(update !== undefined);
        sources.push(update.artifact.source);
        if (update.artifact.source === bad.artifact.source) {
          if (index === 0) {
            throw Object.assign(new Error("This browser refused the update"), { reason: "schema" });
          }
          held = true;
          await gate;
        }
      },
    }),
  );
  let fresh: ReturnType<typeof connectDevelopmentHost> | undefined;
  try {
    await Promise.all(
      connections.map(async (connection) => {
        await connection.ready;
      }),
    );
    await bridge.apply([good], signal());
    let settled = false;
    const refused = assert
      .rejects(
        bridge.apply([bad], signal()),
        (failure: unknown) => failure instanceof HostDeliveryError && failure.reason === "schema",
      )
      .then(() => {
        settled = true;
      });
    await until(() => held);
    assert.equal(settled, false, "a refusal cannot abandon the sibling client's acknowledgement");
    release();
    await refused;
    assert.deepEqual(received, [
      [good.artifact.source, bad.artifact.source],
      [good.artifact.source, bad.artifact.source],
    ]);
    let replay: string | undefined;
    fresh = connectDevelopmentHost({
      url: bridge.url,
      token: bridge.token,
      async apply(artifacts) {
        replay = artifacts[0]?.artifact.source;
      },
    });
    await fresh.ready;
    await until(() => replay !== undefined);
    assert.equal(
      replay,
      good.artifact.source,
      "fresh clients receive the last fully acknowledged snapshot",
    );
    assert.equal(
      received[1]?.length,
      2,
      "the successful browser is not silently reloaded to roll it back",
    );
  } finally {
    release();
    await Promise.all(
      connections.map(async (connection) => {
        await connection.close();
      }),
    );
    await fresh?.close();
    await bridge.close();
  }
});

test("bridge close settles all client waits and queued deliveries before returning", async () => {
  const bridge = await createBrowserBridge();
  const headers = { Authorization: `Bearer ${bridge.token}` };
  try {
    await Promise.all(
      ["left", "right"].map(
        async (client) => await fetch(`${bridge.url}/updates?client=${client}`, { headers }),
      ),
    );
    const first = assert.rejects(bridge.apply([artifact("one")], signal()), { name: "AbortError" });
    const queued = assert.rejects(bridge.apply([artifact("two")], signal()), {
      name: "AbortError",
    });
    const offered = await fetch(`${bridge.url}/updates?client=left`, { headers });
    assert.equal(offered.status, 200);
    await bridge.close();
    await Promise.all([first, queued]);
    await assert.rejects(bridge.apply([artifact("three")], signal()), { name: "AbortError" });
  } finally {
    await bridge.close();
  }
});

test("browser acknowledgement retains structured rejection and close joins an active apply", async () => {
  const bridge = await createBrowserBridge();
  const connection = connectDevelopmentHost({
    url: bridge.url,
    token: bridge.token,
    async apply() {
      throw Object.assign(new Error("A new document is required"), {
        code: "upgrade_conflict",
        reason: "rotation_required",
        message: "A new document is required",
        details: { version: 2 },
      });
    },
  });
  try {
    await connection.ready;
    await assert.rejects(bridge.apply([artifact()], signal()), (error: unknown) => {
      assert.ok(error instanceof HostDeliveryError);
      assert.equal(error.code, "upgrade_conflict");
      assert.equal(error.reason, "rotation_required");
      assert.deepEqual(error.details, { version: 2 });
      return true;
    });
    await connection.close();
    const waiting = bridge.apply([artifact()], signal());
    await bridge.close();
    await assert.rejects(waiting, { name: "AbortError" });
  } finally {
    await connection.close();
    await bridge.close();
  }
});

test("fresh browser clients receive all latest targets without another build", async () => {
  const bridge = await createBrowserBridge();
  await bridge.apply([artifact("one")], signal());
  await bridge.apply([artifact("two")], signal());
  try {
    for (let round = 0; round < 2; round += 1) {
      let targets: string[] = [];
      const connection = connectDevelopmentHost({
        url: bridge.url,
        token: bridge.token,
        async apply(artifacts) {
          targets = artifacts.map((value) => value.target);
        },
      });
      try {
        // oxlint-disable-next-line no-await-in-loop -- Fresh browser clients intentionally connect one after another to test retained state replay.
        await connection.ready;
        // oxlint-disable-next-line no-await-in-loop -- Observe this client's replay before admitting the next client.
        await until(() => targets.length === 2);
        assert.deepEqual(targets, ["one", "two"]);
      } finally {
        // oxlint-disable-next-line no-await-in-loop -- Fully close this client before constructing its replacement.
        await connection.close();
      }
    }
  } finally {
    await bridge.close();
  }
});

test("stdio capability preserves ready events, structured errors, and graceful child cleanup", async () => {
  const events: unknown[] = [];
  const host = createStdioHost({
    command: [
      process.execPath,
      "--input-type=module",
      "-e",
      `
    import {createInterface} from 'node:readline';
    import {writeSync} from 'node:fs';
    console.log(JSON.stringify({ready:true}));
    const input=createInterface({input:process.stdin});
    input.on('line', line => { const {id}=JSON.parse(line); console.log(JSON.stringify({id,ok:false,error:{code:'schema',reason:'incompatible',message:'no update',details:{kept:true}}})); });
    input.on('close',()=>{writeSync(1,JSON.stringify({closed:true})+String.fromCodePoint(10));process.exit(0);});
  `,
    ],
    onEvent(event) {
      events.push(event);
    },
  });
  try {
    await assert.rejects(host.apply([artifact()], signal()), (error: unknown) => {
      assert.ok(error instanceof HostDeliveryError);
      assert.equal(error.code, "schema");
      assert.equal(error.reason, "incompatible");
      assert.deepEqual(error.details, { kept: true });
      return true;
    });
    assert.deepEqual(events, [{ ready: true }]);
  } finally {
    await host.close?.();
  }
  assert.deepEqual(events, [{ ready: true }, { closed: true }]);
  await assert.rejects(host.apply([artifact()], signal()), { name: "AbortError" });
});

test("stdio delivery cancellation settles without a response and missing commands fail", async () => {
  const host = createStdioHost({ command: [process.execPath, "-e", "process.stdin.resume();"] });
  const controller = new AbortController();
  const pending = host.apply([artifact()], controller.signal);
  controller.abort();
  await assert.rejects(pending, { name: "AbortError" });
  await host.close?.();
  const missing = createStdioHost({ command: ["/nonexistent/tembra-development-host"] });
  try {
    await assert.rejects(missing.apply([artifact()], signal()), /ENOENT/u);
  } finally {
    await missing.close?.();
  }
});

test("browser deliveries have bounded acknowledgement waits and reject cancellation", async () => {
  const bridge = await createBrowserBridge({ ackTimeout: 30 });
  const headers = { Authorization: `Bearer ${bridge.token}` };
  try {
    await fetch(`${bridge.url}/updates?client=manual`, { headers });
    await assert.rejects(bridge.apply([artifact()], signal()), /timed out/u);
    const controller = new AbortController();
    const pending = bridge.apply([artifact()], controller.signal);
    controller.abort();
    await assert.rejects(pending, { name: "AbortError" });
  } finally {
    await bridge.close();
  }
});

test("closing the browser helper aborts the embedding apply and joins its cleanup", async () => {
  const bridge = await createBrowserBridge();
  let started = false;
  let settled = false;
  const connection = connectDevelopmentHost({
    url: bridge.url,
    token: bridge.token,
    async apply(_artifacts, signalValue) {
      started = true;
      // oxlint-disable-next-line promise/avoid-new -- The test joins the embedding's physically delayed cancellation cleanup.
      await new Promise<void>((resolve) => {
        signalValue.addEventListener(
          "abort",
          () => {
            settled = true;
            resolve();
          },
          { once: true },
        );
      });
    },
  });
  try {
    await connection.ready;
    const delivery = bridge.apply([artifact()], signal());
    await until(() => started);
    const rejection = assert.rejects(delivery, { name: "AbortError" });
    await connection.close();
    assert.equal(settled, true);
    await bridge.close();
    await rejection;
  } finally {
    await connection.close();
    await bridge.close();
  }
});

test.skipIf(process.platform === "win32")(
  "stdio close removes owned descendants even when their stdio is detached",
  async () => {
    let pid: number | undefined;
    const host = createStdioHost({
      command: [
        process.execPath,
        "--input-type=module",
        "-e",
        `
    import {spawn} from 'node:child_process';
    const child=spawn(process.execPath,['-e','process.on("SIGTERM",()=>{});setInterval(()=>{},1000)'],{stdio:'ignore'});
    console.log(JSON.stringify({ready:true,pid:child.pid}));
    process.stdin.resume(); process.stdin.on('end',()=>process.exit(0));
  `,
      ],
      onEvent(value) {
        if (
          value !== null &&
          typeof value === "object" &&
          "pid" in value &&
          typeof value.pid === "number"
        ) {
          ({ pid } = value);
        }
      },
    });
    try {
      await until(() => pid !== undefined);
      await host.close?.();
      const stopped = async () => {
        try {
          if (pid === undefined) {
            throw new Error("The host did not report a child identifier");
          }
          process.kill(pid, 0);
        } catch {
          return true;
        }
        // Linux supervisors can briefly retain an exited orphan as a zombie.
        const status = await readFile(`/proc/${pid}/stat`, "utf-8").catch(() => "");
        return status.includes(") Z ");
      };
      const deadline = Date.now() + 2000;
      // oxlint-disable-next-line no-await-in-loop -- Check actual descendant liveness after teardown until the operating system reports its exit.
      while (!(await stopped())) {
        if (Date.now() > deadline) {
          assert.fail("The development host left a running descendant");
        }
        // oxlint-disable-next-line no-await-in-loop -- Poll a live transport postcondition until the explicit readiness or cleanup evidence is visible.
        await delay(10);
      }
    } finally {
      await host.close?.();
      if (pid !== undefined) {
        try {
          process.kill(pid, "SIGKILL");
        } catch {
          /* Already stopped. */
        }
      }
    }
  },
);

test("a rejected browser update preserves the acknowledged snapshot for fresh clients", async () => {
  const bridge = await createBrowserBridge();
  const good = artifact();
  const bad = { ...artifact(), artifact: { ...good.artifact, source: "rejected source" } };
  let received = 0;
  const first = connectDevelopmentHost({
    url: bridge.url,
    token: bridge.token,
    async apply(artifacts) {
      received += 1;
      if (artifacts.some((value) => value.artifact.source === bad.artifact.source)) {
        throw new Error("Update refused");
      }
    },
  });
  let fresh: ReturnType<typeof connectDevelopmentHost> | undefined;
  try {
    await first.ready;
    await bridge.apply([good], signal());
    await assert.rejects(bridge.apply([bad], signal()), /Update refused/u);
    await first.close();
    let source: string | undefined;
    fresh = connectDevelopmentHost({
      url: bridge.url,
      token: bridge.token,
      async apply(artifacts) {
        source = artifacts[0]?.artifact.source;
      },
    });
    await fresh.ready;
    await until(() => source !== undefined);
    assert.equal(source, good.artifact.source);
    assert.equal(received, 2);
  } finally {
    await first.close();
    await fresh?.close();
    await bridge.close();
  }
});

test("initial pending browser failures reach client and bridge error observers", async () => {
  const errors: Error[] = [];
  const bridge = await createBrowserBridge({
    onError(error) {
      errors.push(error);
    },
  });
  const original = Object.assign(new Error("Initial install refused"), {
    code: "upgrade_conflict",
    reason: "schema",
  });
  const clientErrors: Error[] = [];
  await bridge.apply([artifact()], signal());
  const connection = connectDevelopmentHost({
    url: bridge.url,
    token: bridge.token,
    async apply() {
      throw original;
    },
    onError(error) {
      clientErrors.push(error);
    },
  });
  try {
    await connection.ready;
    await until(() => errors.length === 1);
    assert.equal(clientErrors[0], original);
    assert.ok(errors[0] instanceof HostDeliveryError);
    assert.equal(errors[0].code, original.code);
    assert.equal(errors[0].reason, original.reason);
  } finally {
    await connection.close();
    await bridge.close();
  }
});
