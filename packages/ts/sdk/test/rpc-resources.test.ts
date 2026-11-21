import { expect, test } from "bun:test";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { client } from "../src/tooling/index.ts";
import { createHost } from "../src/node.ts";
import type { Context } from "../src/runtime/context.ts";
import type { RemoteHost } from "../src/rpc.ts";
import { connect } from "./rpc-resources/connection.ts";

interface Decoder {
  readonly resourceType: "example:rpc-resources/codec@1.0.0#decoder";
  total(ctx?: Context): Promise<bigint>;
  push(frame: bigint, ctx?: Context): Promise<bigint>;
  wait(ctx?: Context): Promise<bigint>;
  next(ctx?: Context): Promise<bigint | null>;
  merge(other: Decoder): Promise<bigint>;
  release(): void;
}
interface Codec {
  readonly reference: { release(): void };
  newDecoder(seed: bigint): Promise<Decoder>;
  decoderZero(): Promise<Decoder>;
  open(seed: bigint, ctx?: Context): Promise<Decoder>;
  consume(value: Decoder, ctx?: Context): Promise<bigint>;
  consumeMany(
    value: [(Decoder | null)[], { tag: "ok"; val: Decoder } | { tag: "err"; val: string }],
  ): Promise<bigint>;
  pair(seed: bigint, ctx?: Context): Promise<[Decoder, (Decoder | null)[]]>;
  stats(): Promise<[number, number]>;
  float(value: number): Promise<number>;
}
type GetCodec = (connection: RemoteHost, address: { orb: string; key: string }) => Promise<Codec>;

/** Generate the actual WIT client, resolving its SDK import to this checkout's source. */
async function generated(shapes = false) {
  const root = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-rpc-client-"),
  );
  const wit = new URL(
    shapes
      ? "../../../../crates/executor-wasm/tests/wit/resources/resources.wit"
      : "../../../../crates/orb-macro/native-fixture/wit/deps/rpc-resources/protocol.wit",
    import.meta.url,
  ).pathname;
  const files = await client({ wit, world: "provider", out: root });
  await writeFile(
    path.join(root, "package.json"),
    JSON.stringify({ name: "tembra-rpc-client-check", type: "module" }),
  );
  await mkdir(path.join(root, "node_modules/@syntropika"), { recursive: true });
  await symlink(
    new URL("../", import.meta.url).pathname,
    path.join(root, "node_modules/@syntropika/tembra-sdk"),
    "dir",
  );
  const file = shapes
    ? files.find((file) => file.endsWith("-sessions.ts"))!
    : files.find((file) => file.endsWith("-codec.ts"))!;
  const { get } = (await import(file)) as { get: GetCodec };
  return {
    get,
    async close() {
      await rm(root, { recursive: true, force: true });
    },
  };
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `generated TypeScript client calls a real ${mode} resource provider over Rust RPC`,
    async () => {
      const source = await generated();
      const connection = connect(mode);
      try {
        const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
        const decoder = await codec.newDecoder(9007199254740993n);
        expect(await decoder.total()).toBe(9007199254740993n);
        const accepted = connection.sent.filter((frame) => frame.op === "accept").length;
        expect(await codec.float(2 ** 80)).toBe(2 ** 80);
        expect(connection.sent.filter((frame) => frame.op === "accept")).toHaveLength(accepted);
        expect(await decoder.push(2n)).toBe(9007199254740995n);
        const zero = await codec.decoderZero();
        expect(await zero.total()).toBe(0n);
        zero.release();
        const other = await codec.open(5n);
        expect(await decoder.merge(other)).toBe(9007199254741000n);
        await expect(other.total()).rejects.toThrow();
        other.release();
        const [first, values] = await codec.pair(8n);
        expect(values[0]).toBeNull();
        expect(await codec.consumeMany([[first, null], { tag: "ok", val: values[1]! }])).toBe(17n);
        await expect(first.total()).rejects.toThrow();
        // Its independent owner preserves the resource after its original service reference ends.
        codec.reference.release();
        expect(await decoder.total()).toBe(9007199254741000n);
        const again = await source.get(connection.remote, { orb: "codec", key: "main" });
        decoder.release();
        expect(await again.consume(await again.open(7n))).toBe(7n);
        again.reference.release();
      } finally {
        try {
          await connection.close();
        } finally {
          await source.close();
        }
      }
    },
    30_000,
  );
}

interface Control {
  stats(): Promise<{ started: number; cancelled: number; references: number; calls: number }>;
  open(): Promise<void>;
  reload(): Promise<number>;
  close(): Promise<void>;
  release(): void;
}

/** Wait for an observable provider condition without assuming an IPC scheduling delay. */
async function until(predicate: () => Promise<boolean>): Promise<void> {
  for (let attempt = 0; attempt < 1000; attempt++) {
    if (await predicate()) return;
    await Bun.sleep(2);
  }
  throw new Error("The real RPC provider did not reach its test barrier");
}

/** Every scenario uses the same generated client and the same RPC server for both executors. */
for (const mode of ["native", "wasm"]) {
  const run = (
    name: string,
    exercise: (
      codec: Codec,
      connection: ReturnType<typeof connect>,
      control: Control,
    ) => Promise<void>,
  ) => {
    test.skipIf(
      !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
    )(
      `${mode} RPC resources: ${name}`,
      async () => {
        const source = await generated();
        const connection = connect(mode);
        try {
          const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
          const control = (await connection.remote.get("control", "main")) as unknown as Control;
          await exercise(codec, connection, control);
          codec.reference.release();
          control.release();
        } finally {
          try {
            await connection.close();
          } finally {
            await source.close();
          }
        }
      },
      30_000,
    );
  };

  run(
    "cancellation before admission leaves ownership and provider code untouched",
    async (codec, _connection, _control) => {
      const application = createHost();
      try {
        const decoder = await codec.open(4n);
        const before = await codec.stats();
        const called = application.root((ctx) => {
          ctx.cancel();
          return codec.consume(decoder, ctx);
        });
        await expect(called).rejects.toBeDefined();
        expect(await codec.stats()).toEqual(before);
        expect(await decoder.total()).toBe(4n);
        decoder.release();
      } finally {
        await application.close();
      }
    },
  );

  run(
    "async next suspends and release waits for the physical method",
    async (codec, _connection, control) => {
      const decoder = await codec.open(9n);
      const before = await codec.stats();
      let done = false;
      const called = decoder.next().then((value) => {
        done = true;
        return value;
      });
      await until(async () => (await control.stats()).started === 1);
      expect(done).toBe(false);
      const retained = (await control.stats()).references;
      decoder.release();
      expect((await control.stats()).references).toBeGreaterThanOrEqual(retained);
      await control.open();
      expect(await called).toBe(10n);
      await until(async () => (await codec.stats())[0] === before[0] + 1);
      await expect(decoder.total()).rejects.toThrow();
    },
  );

  run(
    "cancellation retains a borrowed resource until its physical method settles",
    async (codec, _connection, control) => {
      const application = createHost();
      try {
        const decoder = await codec.open(12n);
        const before = await codec.stats();
        let context: Context | undefined;
        const called = application.root((ctx) => {
          context = ctx;
          return decoder.wait(ctx);
        });
        const outcome = called.then(
          () => undefined,
          (error: unknown) => error,
        );
        await until(async () => (await control.stats()).started === 1);
        context!.cancel();
        expect(await outcome).toBeDefined();
        await until(async () => (await control.stats()).cancelled === 1);
        const retained = (await control.stats()).references;
        decoder.release();
        expect((await control.stats()).references).toBeGreaterThanOrEqual(retained);
        expect((await control.stats()).calls).toBeGreaterThan(1);
        await control.open();
        await until(async () => (await codec.stats())[0] === before[0] + 1);
      } finally {
        await application.close();
      }
    },
  );

  run("a pending receiver cannot also transfer ownership", async (codec, _connection, control) => {
    const decoder = await codec.open(14n);
    const before = await codec.stats();
    const pending = decoder.wait();
    await until(async () => (await control.stats()).started === 1);
    await expect(codec.consume(decoder)).rejects.toBeDefined();
    await expect(decoder.total()).rejects.toThrow();
    await control.open();
    expect(await pending).toBe(14n);
    expect((await codec.stats())[1]).toBe(before[1] + 1);
    await until(async () => (await codec.stats())[0] === before[0] + 1);
    const live = await codec.open(1n);
    expect(await live.total()).toBe(1n);
    live.release();
  });

  run(
    "reload rejects old handles and still allows their release",
    async (codec, _connection, control) => {
      const old = await codec.open(3n);
      await control.reload();
      await expect(old.total()).rejects.toMatchObject({ code: "gone", reason: "resource" });
      old.release();
      old.release();
      const current = await codec.open(4n);
      expect(await current.total()).toBe(4n);
      expect(await codec.consume(current)).toBe(4n);
    },
  );

  run(
    "host closure drains ownership while a resource method is pending",
    async (codec, connection, control) => {
      const decoder = await codec.open(8n);
      const called = decoder.wait().then(
        () => undefined,
        (error: unknown) => error,
      );
      await until(async () => (await control.stats()).started === 1);
      await control.close().catch(() => {});
      expect(await called).toBeDefined();
      await connection.output;
      await expect(decoder.total()).rejects.toMatchObject({ code: "gone", reason: "connection" });
      decoder.release();
    },
  );

  run(
    "disconnect revokes handles and settles a pending method",
    async (codec, connection, control) => {
      const decoder = await codec.open(8n);
      const called = decoder.wait().then(
        () => undefined,
        (error: unknown) => error,
      );
      await until(async () => (await control.stats()).started === 1);
      connection.remote.close();
      expect(await called).toBeDefined();
      await expect(decoder.total()).rejects.toMatchObject({ code: "gone", reason: "connection" });
      decoder.release();
    },
  );

  run(
    "a cancelled factory reply closes both late and unaccepted delivery",
    async (codec, connection) => {
      const application = createHost();
      try {
        const before = await codec.stats();
        connection.hold((frame) => {
          const result = frame.ok as { owned?: unknown[] } | undefined;
          return Boolean(result?.owned?.length);
        });
        let context: Context | undefined;
        const pending = application.root((ctx) => {
          context = ctx;
          return codec.open(21n, ctx);
        });
        const outcome = pending.then(
          () => undefined,
          (error: unknown) => error,
        );
        await until(async () =>
          connection.received.some((frame) =>
            Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
          ),
        );
        context!.cancel();
        expect(await outcome).toBeDefined();
        await until(async () => (await codec.stats())[0] === before[0] + 1);
        connection.flush();
        await connection.remote.installedOrbs();
        expect((await codec.stats())[0]).toBe(before[0] + 1);
      } finally {
        connection.flush();
        await application.close();
      }
    },
  );

  run("a local Get refusal leaves existing resource owners usable", async (codec, connection) => {
    const decoder = await codec.open(17n);
    const sent = connection.sent.length;
    await expect(
      connection.remote.get(
        "codec::example:rpc-resources/codec@1.0.0",
        "x".repeat(connection.remote.channel.maxFrame + 1),
      ),
    ).rejects.toMatchObject({ code: "exhausted", reason: "frame_size" });
    expect(connection.sent).toHaveLength(sent);
    expect(connection.remote.channel.closed).toBe(false);
    expect(await decoder.total()).toBe(17n);
    const reference = await connection.remote.get(
      "codec::example:rpc-resources/codec@1.0.0",
      "after-local-failure",
    );
    reference.release();
    decoder.release();
  });

  run(
    "a cancelled Get with a failed delivery acknowledgement releases every server claim",
    async (_codec, connection) => {
      const controller = new AbortController();
      connection.hold((frame) => Boolean((frame.ok as { reference?: unknown })?.reference));
      const send = connection.failSend((frame) => frame.op === "get");
      const called = connection.remote.get(
        "codec::example:rpc-resources/codec@1.0.0",
        "uncertain",
        { signal: controller.signal },
      );
      const outcome = called.catch((error: unknown) => error);
      await send.started;
      const id = connection.sent.at(-1)!.id;
      await until(async () =>
        connection.received.some(
          (frame) => frame.id === id && Boolean((frame.ok as { reference?: unknown })?.reference),
        ),
      );
      controller.abort();
      expect(await outcome).toMatchObject({ name: "AbortError" });
      send.reject(new Error("Get delivery acknowledgement lost"));
      await until(async () => connection.remote.channel.closed);
      expect(
        connection.sent.filter(
          (frame) =>
            frame.op === "get" &&
            Array.isArray(frame.payload?.key) &&
            frame.payload.key[0] === "uncertain",
        ),
      ).toHaveLength(1);
      connection.flush();
      // The run's close receipt requires zero references, calls and roots in the real Rust host.
    },
  );

  run(
    "a failed send closes uncertain delivery even after cancellation",
    async (codec, connection) => {
      const application = createHost();
      try {
        connection.hold((frame) =>
          Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
        );
        const send = connection.failSend(
          (frame) => frame.op === "call" && frame.payload?.method === "open",
        );
        let context: Context | undefined;
        const called = application.root((ctx) => {
          context = ctx;
          return codec.open(6n, ctx);
        });
        const outcome = called.then(
          () => undefined,
          (error: unknown) => error,
        );
        await send.started;
        context!.cancel();
        expect(await outcome).toBeDefined();
        send.reject(new Error("test sender lost its delivery acknowledgement"));
        await until(async () => connection.remote.channel.closed);
        expect(
          connection.sent.filter(
            (frame) => frame.op === "call" && frame.payload?.method === "open",
          ),
        ).toHaveLength(1);
      } finally {
        connection.flush();
        await application.close();
      }
    },
  );

  run("an uncertain owned transfer is never restored or replayed", async (codec, connection) => {
    const application = createHost();
    try {
      const decoder = await codec.open(31n);
      const before = await codec.stats();
      connection.hold((frame) => (frame.ok as { output?: unknown } | undefined)?.output === '"31"');
      let context: Context | undefined;
      const pending = application.root((ctx) => {
        context = ctx;
        return codec.consume(decoder, ctx);
      });
      const outcome = pending.then(
        () => undefined,
        (error: unknown) => error,
      );
      await until(async () =>
        connection.received.some(
          (frame) => (frame.ok as { output?: unknown } | undefined)?.output === '"31"',
        ),
      );
      context!.cancel();
      expect(await outcome).toBeDefined();
      connection.flush();
      await expect(decoder.total()).rejects.toThrow();
      await until(async () => (await codec.stats())[0] === before[0] + 1);
      expect((await codec.stats())[1]).toBe(before[1] + 1);
      expect(
        connection.sent.filter(
          (frame) => frame.op === "call" && frame.payload?.method === "consume",
        ),
      ).toHaveLength(1);
      decoder.release();
    } finally {
      connection.flush();
      await application.close();
    }
  });

  run(
    "release revokes a handle before the following frame is admitted",
    async (codec, connection) => {
      const decoder = await codec.open(3n);
      const acquired = connection.received.find(
        (frame) => (frame.ok as { resources?: unknown } | undefined)?.resources === 1,
      )!.ok as { signature: string };
      const result = connection.received.find((frame) =>
        Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
      )!.ok as { owned: Record<string, unknown>[] };
      const descriptor = result.owned[0]!;
      const before = await codec.stats();
      const released = connection.remote.request("release", {
        reference: descriptor.reference,
        resource: descriptor,
      });
      const called = connection.remote.request("call", {
        reference: descriptor.reference,
        method: "[method]decoder.total",
        input: "[]",
        capabilities: { signature: acquired.signature, receiver: descriptor, owned: [] },
      });
      await released;
      await expect(called).rejects.toMatchObject({ code: "forbidden", reason: "method" });
      const after = await codec.stats();
      expect(after[1]).toBe(before[1]);
      expect(after[0]).toBe(before[0] + 1);
      decoder.release();
    },
  );

  run(
    "unsupported and forged capability requests never enter provider code",
    async (codec, connection) => {
      const acquired = connection.received.find(
        (frame) => (frame.ok as { resources?: unknown } | undefined)?.resources === 1,
      )!.ok as { reference: number; signature: string };
      const before = await codec.stats();
      await expect(
        connection.remote.request("call", {
          reference: acquired.reference,
          method: "open",
          arguments: [1],
        }),
      ).rejects.toMatchObject({ code: "unsupported", reason: "capabilities" });
      const decoder = await codec.open(2n);
      const response = connection.received.find((frame) =>
        Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
      )!.ok as { owned: Record<string, unknown>[] };
      const descriptor = response.owned[0]!;
      const entered = await codec.stats();
      // A resource ID never substitutes for the released service reference or its descriptor.
      await expect(
        connection.remote.request("call", {
          reference: descriptor.reference,
          method: "float",
          arguments: [1],
        }),
      ).rejects.toMatchObject({ code: "unsupported", reason: "capabilities" });
      await expect(
        connection.remote.request("call", {
          reference: descriptor.reference,
          method: "[method]decoder.total",
          input: "[]",
          capabilities: {
            signature: acquired.signature,
            receiver: { ...descriptor, token: "0".repeat(64) },
            owned: [],
          },
        }),
      ).rejects.toMatchObject({ code: "forbidden", reason: "resource" });
      await expect(
        connection.remote.request("call", {
          reference: acquired.reference,
          method: "consume",
          input: "[0]",
          capabilities: {
            signature: acquired.signature,
            receiver: null,
            owned: [{ ...descriptor, revision: "999999" }],
          },
        }),
      ).rejects.toBeDefined();
      expect((await codec.stats())[1]).toBe(entered[1]);
      expect(entered[1]).toBe(before[1] + 1);
      decoder.release();
    },
  );
}

interface SessionResource {
  total(): Promise<bigint>;
  release(): void;
}
interface Bundle {
  primary: SessionResource;
  optional: SessionResource | null;
  extras: SessionResource[];
  count: { release(): void };
  seed: bigint;
  label: string;
}
interface Shapes {
  reference: { release(): void };
  openBundle(
    seed: bigint,
    fail: boolean,
  ): Promise<{ tag: "ok"; val: Bundle } | { tag: "err"; val: string }>;
  takeBundle(bundle: Bundle): Promise<bigint[]>;
  choose(seed: bigint): Promise<{ tag: "empty" } | { tag: "active"; val: SessionResource }>;
  closed(): Promise<bigint>;
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_SHAPES_COMPONENT),
  )(
    `${mode} RPC resources: records, variants, results and wide integers retain ownership`,
    async () => {
      const source = await generated(true);
      const connection = connect(mode);
      try {
        const get = source.get as unknown as (
          remote: RemoteHost,
          address: { orb: string; key: string },
        ) => Promise<Shapes>;
        const shapes = await get(connection.remote, { orb: "shapes", key: "main" });
        const seed = 9007199254740993n;
        const opened = await shapes.openBundle(seed, false);
        expect(opened.tag).toBe("ok");
        if (opened.tag !== "ok") throw new Error(opened.val);
        expect(opened.val.seed).toBe(seed);
        expect(await opened.val.primary.total()).toBe(seed);
        expect(await opened.val.optional!.total()).toBe(seed + 1n);
        expect(await shapes.takeBundle(opened.val)).toEqual([
          seed,
          seed + 1n,
          seed + 2n,
          seed + 3n,
          1n,
          seed,
        ]);
        await expect(opened.val.primary.total()).rejects.toThrow();
        expect(await shapes.closed()).toBe(4n);
        expect(await shapes.openBundle(seed, true)).toEqual({ tag: "err", val: "refused" });
        expect(await shapes.choose(0n)).toEqual({ tag: "empty" });
        const selected = await shapes.choose(seed);
        expect(selected.tag).toBe("active");
        if (selected.tag === "active") {
          expect(await selected.val.total()).toBe(seed + 1n);
          selected.val.release();
        }
        await until(async () => (await shapes.closed()) === 5n);
        shapes.reference.release();
      } finally {
        try {
          await connection.close();
        } finally {
          await source.close();
        }
      }
    },
    30_000,
  );
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `${mode} RPC resources: a handle from another connection grants no authority`,
    async () => {
      const source = await generated();
      const first = connect(mode);
      const second = connect(mode);
      try {
        const a = await source.get(first.remote, { orb: "codec", key: "main" });
        const b = await source.get(second.remote, { orb: "codec", key: "main" });
        const ownedA = await a.open(1n);
        const ownedB = await b.open(2n);
        const acquired = second.received.find(
          (frame) => (frame.ok as { resources?: unknown } | undefined)?.resources === 1,
        )!.ok as { signature: string };
        const delivery = first.received.find((frame) =>
          Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
        )!.ok as { owned: Record<string, unknown>[] };
        const descriptor = delivery.owned[0]!;
        const before = await b.stats();
        await expect(
          second.remote.request("call", {
            reference: descriptor.reference,
            method: "[method]decoder.total",
            input: "[]",
            capabilities: { signature: acquired.signature, receiver: descriptor, owned: [] },
          }),
        ).rejects.toMatchObject({ code: "forbidden", reason: "resource" });
        expect((await b.stats())[1]).toBe(before[1]);
        expect(await ownedB.total()).toBe(2n);
        ownedA.release();
        ownedB.release();
        a.reference.release();
        b.reference.release();
      } finally {
        try {
          await first.close();
          await second.close();
        } finally {
          await source.close();
        }
      }
    },
    30_000,
  );

  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `${mode} RPC resources: failed result acquisition closes every unpublished owner`,
    async () => {
      const source = await generated();
      const connection = connect(mode, { maxReferences: 3 });
      try {
        const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
        const held = await codec.open(5n);
        const before = await codec.stats();
        await expect(codec.pair(9n)).rejects.toMatchObject({
          code: "exhausted",
          reason: "remote_references",
        });
        expect((await codec.stats())[0]).toBe(before[0] + 2);
        expect(await held.total()).toBe(5n);
        held.release();
        codec.reference.release();
      } finally {
        try {
          await connection.close();
        } finally {
          await source.close();
        }
      }
    },
    30_000,
  );

  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_SHAPES_COMPONENT),
  )(
    `${mode} RPC resources: an oversized result closes its attachments and keeps the connection usable`,
    async () => {
      const source = await generated(true);
      const connection = connect(mode, { maxFrame: 1024 });
      try {
        const get = source.get as unknown as (
          remote: RemoteHost,
          address: { orb: string; key: string },
        ) => Promise<Shapes & { openPadded(seed: bigint, padding: number): Promise<Bundle> }>;
        const shapes = await get(connection.remote, { orb: "shapes", key: "main" });
        await expect(shapes.openPadded(1n, 10_000)).rejects.toMatchObject({
          code: "exhausted",
          reason: "frame_size",
        });
        expect(await shapes.closed()).toBe(4n);
        shapes.reference.release();
      } finally {
        try {
          await connection.close();
        } finally {
          await source.close();
        }
      }
    },
    30_000,
  );
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `${mode} RPC resources: release and failed owned submissions survive ordinary request saturation`,
    async () => {
      const source = await generated();
      const connection = connect(mode, { maxPending: 2 });
      try {
        const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
        const control = (await connection.remote.get("control", "main")) as unknown as Control;
        const borrowed = await codec.open(1n);
        const released = await codec.open(2n);
        const failed = await codec.open(3n);
        const before = await codec.stats();
        let settled = false;
        const pending = borrowed.wait().then((value) => {
          settled = true;
          return value;
        });
        await until(async () => (await control.stats()).started === 1);
        connection.hold(
          (frame) => typeof frame.ok === "object" && frame.ok !== null && "started" in frame.ok,
        );
        const other = control.stats();
        const responses = connection.received.length;
        await until(async () =>
          connection.received
            .slice(responses)
            .some(
              (frame) => typeof frame.ok === "object" && frame.ok !== null && "started" in frame.ok,
            ),
        );
        released.release();
        await expect(codec.consume(failed)).rejects.toMatchObject({
          code: "exhausted",
          reason: "pending_requests",
        });
        await until(
          async () => connection.received.filter((frame) => frame.ok === null).length >= 2,
        );
        expect(connection.remote.channel.closed).toBe(false);
        expect(settled).toBe(false);
        connection.flush();
        expect((await other).started).toBe(1);
        await control.open();
        expect(await pending).toBe(1n);
        expect(await codec.stats()).toEqual([before[0] + 2, before[1] + 1]);
        await expect(failed.total()).rejects.toThrow();
        borrowed.release();
        await connection.remote.installedOrbs();
        codec.reference.release();
        control.release();
      } finally {
        await connection.close();
        await source.close();
      }
    },
    30_000,
  );
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `${mode} RPC resources: cancelled late container replies release without consuming ordinary slots`,
    async () => {
      const source = await generated();
      const connection = connect(mode, { maxPending: 2 });
      const application = createHost();
      try {
        const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
        const control = (await connection.remote.get("control", "main")) as unknown as Control;
        const borrowed = await codec.open(1n);
        const before = await codec.stats();
        connection.hold((frame) =>
          Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
        );
        let context: Context | undefined;
        const factory = application.root((ctx) => {
          context = ctx;
          return codec.pair(7n, ctx);
        });
        const outcome = factory.catch((error: unknown) => error);
        const received = connection.received.length;
        await until(async () =>
          connection.received
            .slice(received)
            .some((frame) =>
              Boolean((frame.ok as { owned?: unknown[] } | undefined)?.owned?.length),
            ),
        );
        context!.cancel();
        expect(await outcome).toBeDefined();
        let settled = false;
        const pending = borrowed.wait().then((value) => {
          settled = true;
          return value;
        });
        connection.flush();
        await until(async () => (await control.stats()).started === 1);
        expect(connection.remote.channel.closed).toBe(false);
        expect(settled).toBe(false);
        await control.open();
        expect(await pending).toBe(1n);
        expect((await codec.stats())[0]).toBe(before[0] + 2);
        expect(
          connection.sent.filter((frame) => frame.op === "release" && frame.payload?.resource),
        ).toHaveLength(2);
        borrowed.release();
        await connection.remote.installedOrbs();
        codec.reference.release();
        control.release();
      } finally {
        connection.flush();
        await connection.close();
        await application.close();
        await source.close();
      }
    },
    30_000,
  );
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `${mode} RPC resources: a stalled release byte queue closes with bounded cleanup`,
    async () => {
      const source = await generated();
      const connection = connect(mode, { maxPending: 1, maxFrame: 1024 });
      try {
        const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
        const first = await codec.open(1n);
        const queued = [];
        for (let index = 0; index < 12; index++) queued.push(await codec.open(2n));
        const retained = await codec.open(3n);
        connection.hold((frame) => frame.ok === null);
        first.release();
        await until(async () => connection.received.some((frame) => frame.ok === null));
        for (const resource of queued) resource.release();
        await until(async () => connection.remote.channel.closed);
        await expect(retained.total()).rejects.toMatchObject({
          code: "gone",
          reason: "connection",
        });
        expect(connection.sent.filter((frame) => frame.op === "release")).toHaveLength(1);
        retained.release();
        codec.reference.release();
      } finally {
        connection.flush();
        await connection.close();
        await source.close();
      }
    },
    30_000,
  );
}

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `${mode} RPC resources: a release burst drains while an unrelated method stays pending`,
    async () => {
      const source = await generated();
      const connection = connect(mode, { maxPending: 2, maxReferences: 64 });
      try {
        const codec = await source.get(connection.remote, { orb: "codec", key: "main" });
        const control = (await connection.remote.get("control", "main")) as unknown as Control;
        const owners = [];
        for (let index = 0; index < 24; index++) owners.push(await codec.open(BigInt(index)));
        const before = await codec.stats();
        let settled = false;
        const pending = owners[0]!.wait().then((value) => {
          settled = true;
          return value;
        });
        await until(async () => (await control.stats()).started === 1);
        connection.hold((frame) => frame.ok === null);
        for (const owner of owners) owner.release();
        await until(
          async () => connection.received.filter((frame) => frame.ok === null).length >= 2,
        );
        expect(connection.sent.filter((frame) => frame.op === "release")).toHaveLength(2);
        expect(connection.remote.channel.closed).toBe(false);
        expect(settled).toBe(false);
        connection.flush();
        await until(
          async () => connection.received.filter((frame) => frame.ok === null).length >= 24,
        );
        expect(connection.sent.filter((frame) => frame.op === "release")).toHaveLength(24);
        expect((await control.stats()).started).toBe(1);
        expect(settled).toBe(false);
        await control.open();
        expect(await pending).toBe(0n);
        expect((await codec.stats())[0]).toBe(before[0] + 24);
        codec.reference.release();
        control.release();
      } finally {
        connection.flush();
        await connection.close();
        await source.close();
      }
    },
    30_000,
  );
}
