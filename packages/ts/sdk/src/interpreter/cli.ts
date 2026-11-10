import type { CliOptions } from "../authoring/types.ts";

/** Capture installation data before asynchronous preparation can observe caller mutations. */
export function captureCli(options: CliOptions): CliOptions {
  const input = options.stdin;
  return {
    ...options,
    ...(options.arguments ? { arguments: [...options.arguments] } : {}),
    ...(options.environment
      ? { environment: options.environment.map(([key, value]) => [key, value] as const) }
      : {}),
    ...(typeof input === "object"
      ? { stdin: "bytes" in input ? { bytes: input.bytes.slice() } : { ...input } }
      : {}),
    ...(typeof options.stdout === "object" ? { stdout: { ...options.stdout } } : {}),
    ...(typeof options.stderr === "object" ? { stderr: { ...options.stderr } } : {}),
  };
}

/** Copy explicitly configured stdin once into the native installation's data configuration. */
export function cliPacket(options: CliOptions | undefined): string {
  if (!options) return "{}";
  const stdin = options.stdin;
  return JSON.stringify({
    ...options,
    ...(typeof stdin === "object" && "bytes" in stdin
      ? { stdin: { bytes: Array.from(stdin.bytes) } }
      : {}),
  });
}
