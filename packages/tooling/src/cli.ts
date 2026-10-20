#!/usr/bin/env bun
import { once } from "node:events";
import { inspect } from "node:util";

import { build } from "./build.js";
import { registerEmbeddedPackages } from "./cli-modules.js";
import { loadConfig, loadTrustedModule } from "./config.js";
import { dev } from "./dev.js";
import { ToolingError } from "./error.js";
import type { DevelopmentHost, Platform, ResolvedConfig } from "./types.js";

/**
 * Run `build` or `dev` with optional `--config <file>`; defaults to process arguments after the bin.
 * Build prints artifact paths. Development loads the configured trusted adapter and remains active
 * until SIGINT/SIGTERM, then joins session cleanup. Invalid adapter maps first join cleanup of hosts
 * already accepted during validation. Failures reject with their original cause chain; the executable
 * entrypoint formats them and sets a failing exit status. Not exported by the package.
 */
export async function runCli(args: readonly string[] = process.argv.slice(2)): Promise<void> {
  const [operation, ...options] = args;
  if (operation === "--help" || operation === "-h") {
    process.stdout.write("usage: tembra <build|dev> [--config file]\n");
    return;
  }
  if (operation !== "build" && operation !== "dev") {
    throw new ToolingError("config", "usage", "usage: tembra <build|dev> [--config file]");
  }
  let configFile: string | undefined;
  for (let index = 0; index < options.length; index += 2) {
    const option = options[index];
    const file = options[index + 1];
    if (
      option !== "--config" ||
      configFile !== undefined ||
      file === undefined ||
      file.length === 0 ||
      file.startsWith("--")
    ) {
      throw new ToolingError("config", "usage", "usage: tembra <build|dev> [--config file]");
    }
    configFile = file;
  }
  const config = await loadConfig(configFile);
  const controller = new AbortController();
  const stop = () => {
    controller.abort(new Error("CLI shutdown requested"));
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
  try {
    if (operation === "build") {
      const result = await build(config, { signal: controller.signal });
      for (const artifact of result.artifacts) {
        process.stdout.write(`${artifact.target}: ${artifact.artifactPath}\n`);
      }
      return;
    }
    const hosts = await createDevelopmentHosts(config);
    const session = await dev(config, {
      hosts,
      signal: controller.signal,
      onBuild: (result) => {
        process.stdout.write(
          `built ${result.artifacts.map((artifact) => artifact.target).join(", ")}\n`,
        );
      },
      onReload: (event) => {
        process.stdout.write(`${event.platform}: ${event.outcome.status}\n`);
      },
      onError: (error) => {
        process.stderr.write(`${formatError(error)}\n`);
      },
    });
    try {
      await session.ready;
      if (!controller.signal.aborted) {
        await once(controller.signal, "abort");
      }
    } finally {
      await session.close();
    }
  } finally {
    process.removeListener("SIGINT", stop);
    process.removeListener("SIGTERM", stop);
  }
}

function isHost(value: unknown): value is DevelopmentHost {
  return (
    value !== null &&
    typeof value === "object" &&
    "apply" in value &&
    typeof value.apply === "function" &&
    (!("close" in value) || value.close === undefined || typeof value.close === "function")
  );
}
function isHostFactory(value: unknown): value is (config: ResolvedConfig) => unknown {
  return typeof value === "function";
}
async function createDevelopmentHosts(
  config: ResolvedConfig,
): Promise<Partial<Readonly<Record<Platform, DevelopmentHost>>>> {
  if (config.dev.adapter === undefined) {
    return {};
  }
  const hosts: Partial<Record<Platform, DevelopmentHost>> = {};
  try {
    const adapter = await loadTrustedModule(config.dev.adapter);
    const { createHosts } = adapter.exports;
    if (!isHostFactory(createHosts)) {
      throw new Error("Development adapter must export createHosts(config)");
    }
    const value: unknown = await createHosts(config);
    if (value === null || typeof value !== "object") {
      throw new Error("createHosts(config) must return a development host map");
    }
    for (const platform of ["server", "browser"] as const) {
      if (!(platform in value)) {
        continue;
      }
      const host: unknown = Reflect.get(value, platform);
      if (host === undefined) {
        continue;
      }
      if (!isHost(host)) {
        throw new Error(`Development ${platform} host must implement apply(artifacts, signal)`);
      }
      hosts[platform] = host;
    }
    return hosts;
  } catch (error) {
    const outcomes = await Promise.allSettled(
      [...new Set(Object.values(hosts))].map(async (host) => {
        await host.close?.();
      }),
    );
    const failures: unknown[] = [];
    for (const outcome of outcomes) {
      if (outcome.status === "rejected") {
        failures.push(outcome.reason);
      }
    }
    const cause =
      failures.length === 0
        ? error
        : new AggregateError(failures, "Unable to close accepted development hosts", {
            cause: error,
          });
    throw new ToolingError(
      "config",
      "adapter_failed",
      "Unable to load development adapter",
      undefined,
      { cause },
    );
  }
}

function formatError(error: unknown, seen = new Set<unknown>()): string {
  if (seen.has(error)) {
    return "[circular cause]";
  }
  if (
    error === null ||
    typeof error !== "object" ||
    !("message" in error) ||
    typeof error.message !== "string"
  ) {
    return inspect(error, { depth: 5, colors: false });
  }
  seen.add(error);
  const code = "code" in error ? error.code : undefined;
  const reason = "reason" in error ? error.reason : undefined;
  const detail = "details" in error ? error.details : undefined;
  const category =
    error instanceof ToolingError
      ? `${error.phase}/${error.reason}`
      : [code, reason].filter((value) => typeof value === "string").join("/");
  const prefix = category ? `${category}: ` : "";
  const details =
    detail === undefined ? "" : `\ndetails: ${inspect(detail, { depth: 5, colors: false })}`;
  const recovery = category.includes("rotation_required")
    ? "\nStart a new application process or browser document. The new session does not retain existing session references."
    : "";
  const nested =
    "errors" in error && Array.isArray(error.errors)
      ? error.errors
          .map(
            (failure: unknown, index: number) =>
              `\nerror ${index + 1}: ${formatError(failure, seen)}`,
          )
          .join("")
      : "";
  const cause = "cause" in error ? error.cause : undefined;
  return `${prefix}${error.message}${details}${recovery}${nested}${cause === undefined ? "" : `\ncaused by: ${formatError(cause, seen)}`}`;
}
/** Run the executable boundary with the same diagnostics from the package bin or direct module. */
export async function main(): Promise<void> {
  try {
    registerEmbeddedPackages();
    await runCli();
  } catch (error) {
    process.stderr.write(`${formatError(error)}\n`);
    process.exitCode = 1;
  }
}

if (import.meta.main) {
  await main();
}
