/** Generated data-only clients use checked WIT delivery over actual Rust RPC. */
import { expect, test } from "bun:test";
import { mkdtemp, mkdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { client } from "../src/tooling/index.ts";
import { createHost } from "../src/node.ts";
import { orb } from "../src/orb.ts";
import type { Context } from "../src/runtime/context.ts";
import type { DynamicReference } from "../src/authoring/types.ts";
import { connect } from "./rpc-resources/connection.ts";

interface Limits {
  unsignedMax: bigint;
  signedMin: bigint;
  signedMax: bigint;
}
type Choice = { tag: "empty" } | { tag: "limits"; val: Limits };
type Outcome = { tag: "ok"; val: Limits } | { tag: "err"; val: bigint };
type Nested = [(readonly [bigint, bigint] | null)[], Choice, Outcome];
interface Values {
  readonly reference: { release(): void };
  echo(value: Limits): Promise<Limits>;
  wait(value: Limits, context?: Context): Promise<Limits>;
  nested(values: Nested[0], choice: Choice, outcome: Outcome): Promise<Nested>;
}
type Get = (
  host: { get(service: string, key: string | readonly string[]): Promise<DynamicReference> },
  address: { orb: string; key: string },
) => Promise<Values>;

for (const mode of ["native", "wasm"]) {
  test.skipIf(
    !process.env.TEMBRA_RPC_HOST || (mode === "wasm" && !process.env.TEMBRA_RPC_COMPONENT),
  )(
    `generated data-only client preserves all integer boundaries over ${mode} Rust RPC`,
    async () => {
      const root = await mkdtemp(
        path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-rpc-data-"),
      );
      const connection = connect(mode);
      const application = createHost();
      try {
        const wit = new URL(
          "../../../../crates/orb-macro/native-fixture/wit/deps/rpc-resources/protocol.wit",
          import.meta.url,
        ).pathname;
        const files = await client({ wit, world: "provider", out: root });
        await writeFile(
          path.join(root, "package.json"),
          JSON.stringify({ name: "tembra-rpc-data-check", type: "module" }),
        );
        await mkdir(path.join(root, "node_modules/@syntropika"), { recursive: true });
        await symlink(
          new URL("../", import.meta.url).pathname,
          path.join(root, "node_modules/@syntropika/tembra-sdk"),
          "dir",
        );
        const { get } = (await import(files.find((file) => file.endsWith("-values.ts"))!)) as {
          get: Get;
        };
        const values = await get(connection.remote, { orb: "codec", key: "main" });
        const limits = {
          unsignedMax: 18446744073709551615n,
          signedMin: -9223372036854775808n,
          signedMax: 9223372036854775807n,
        };
        expect(await values.echo(limits)).toEqual(limits);
        expect(
          await values.nested(
            [null, [0n, -9223372036854775808n], [18446744073709551615n, 9223372036854775807n]],
            { tag: "limits", val: limits },
            { tag: "ok", val: limits },
          ),
        ).toEqual([
          [null, [0n, -9223372036854775808n], [18446744073709551615n, 9223372036854775807n]],
          { tag: "limits", val: limits },
          { tag: "ok", val: limits },
        ]);
        expect(
          await values.nested([], { tag: "empty" }, { tag: "err", val: -9223372036854775808n }),
        ).toEqual([[], { tag: "empty" }, { tag: "err", val: -9223372036854775808n }]);
        const control = await connection.remote.get("control", "main");
        await expect(
          get(connection.remote, { orb: "unprepared", key: "main" }),
        ).rejects.toMatchObject({ code: "unsupported", reason: "capabilities" });
        expect(await control["unprepared-calls"]()).toBe(0);
        let context: Context | undefined;
        const waiting = application.root((ctx) => {
          context = ctx;
          return values.wait(limits, ctx);
        });
        const cancelled = waiting.then(
          () => false,
          () => true,
        );
        for (let tries = 0; (await control.stats()).started !== 1; tries++) {
          if (tries === 1000) throw new Error("Data call did not reach its imported wait");
          await Bun.sleep(1);
        }
        context!.cancel();
        expect(await cancelled).toBe(true);
        await control.open();
        expect(await values.echo(limits)).toEqual(limits);
        await application.install(
          orb({
            name: "local",
            apiVersion: 1,
            services: {
              "local::example:rpc-resources/values@1.0.0": {
                create: () => ({}),
                methods: {
                  echo: (_state, _context, value: Limits) => value,
                },
              },
            },
          }),
        );
        const local = await get(application, { orb: "local", key: "main" });
        expect(await local.echo(limits)).toEqual(limits);
        local.reference.release();
        control.release();
        values.reference.release();
      } finally {
        await connection.close();
        await application.close();
        await rm(root, { recursive: true, force: true });
      }
    },
    30_000,
  );
}
