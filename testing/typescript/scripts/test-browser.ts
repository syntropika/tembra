import { spawn } from "node:child_process";
import { once } from "node:events";
import { createInterface } from "node:readline";

const executable = process.env.AGENT_BROWSER ?? "agent-browser";
const session = `tembra-host-${process.pid}`;
const server = spawn(process.execPath, ["scripts/browser-test-server.ts"], {
  cwd: new URL("../", import.meta.url),
  stdio: ["ignore", "pipe", "inherit"],
});
if (server.stdout === null) {
  throw new Error("browser test server has no output");
}
const lines = createInterface({ input: server.stdout });
async function browser(...args: string[]): Promise<string> {
  const process = Bun.spawn([executable, "--session", session, ...args], {
    stdout: "pipe",
    stderr: "inherit",
  });
  const text = await new Response(process.stdout).text();
  if ((await process.exited) !== 0) {
    throw new Error(`browser command failed: ${args[0]}`);
  }
  return text;
}
try {
  const announcement: readonly unknown[] = await Promise.race([
    once(lines, "line"),
    once(server, "exit").then(() => {
      throw new Error("browser test server exited before readiness");
    }),
  ]);
  const [url] = announcement;
  if (typeof url !== "string") {
    throw new TypeError("browser test server did not return a URL");
  }
  await browser("open", url);
  for (let attempt = 0; attempt < 100; attempt += 1) {
    // Each observation follows the previous browser operation and pending WASM work.
    // oxlint-disable-next-line no-await-in-loop -- Each observation must wait for the preceding browser operation.
    const text = await browser("get", "text", "output");
    const result = text.trim();
    if (result !== "running") {
      const outcome: unknown = JSON.parse(result);
      if (outcome === null || typeof outcome !== "object" || !("passed" in outcome)) {
        throw new Error(result);
      }
      console.log(result);
      break;
    }
    if (attempt === 99) {
      throw new Error("browser acceptance timed out");
    }
    // oxlint-disable-next-line no-await-in-loop -- Poll after allowing WASM and the previous observation to progress.
    await Bun.sleep(100);
  }
} finally {
  try {
    await browser("close");
  } finally {
    lines.close();
    server.kill();
  }
}
