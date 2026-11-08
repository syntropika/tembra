import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

/** Run one task-owned Chrome profile and always stop the process before deleting its data. */
export async function headless(url: string, pass: string, fail: string): Promise<void> {
  const profile = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-browser-"),
  );
  let browser: ReturnType<typeof Bun.spawn> | undefined;
  let socket: WebSocket | undefined;
  const deadline = setTimeout(() => browser?.kill(), 60000);
  try {
    browser = Bun.spawn(
      [
        process.env.TEMBRA_BROWSER ?? "google-chrome",
        "--headless=new",
        "--disable-gpu",
        `--user-data-dir=${profile}`,
        "--remote-debugging-port=0",
        "--no-first-run",
        "about:blank",
      ],
      { stdout: "ignore", stderr: "pipe" },
    );
    const reader = (browser.stderr as ReadableStream<Uint8Array>).getReader();
    let diagnostic = "";
    let endpoint: string | undefined;
    while (!endpoint) {
      const chunk = await reader.read();
      if (chunk.done) throw new Error(`Browser exited before startup: ${diagnostic}`);
      diagnostic += new TextDecoder().decode(chunk.value);
      endpoint = diagnostic.match(/DevTools listening on (ws:\/\/\S+)/)?.[1];
    }
    socket = new WebSocket(endpoint);
    await new Promise<void>((resolve, reject) => {
      socket!.onopen = () => resolve();
      socket!.onerror = reject;
    });
    let next = 0;
    const pending = new Map<number, { resolve(value: any): void; reject(error: unknown): void }>();
    const consoleMessages: string[] = [];
    socket.onmessage = (event) => {
      const message = JSON.parse(String(event.data));
      const request = pending.get(message.id);
      if (message.method === "Runtime.consoleAPICalled")
        consoleMessages.push(JSON.stringify(message.params.args));
      if (!request) return;
      pending.delete(message.id);
      if (message.error) request.reject(new Error(JSON.stringify(message.error)));
      else request.resolve(message.result);
    };
    socket.onclose = () => {
      for (const request of pending.values())
        request.reject(new Error("Browser connection closed"));
      pending.clear();
    };
    const command = (method: string, params: unknown = {}, sessionId?: string) =>
      new Promise<any>((resolve, reject) => {
        const id = ++next;
        pending.set(id, { resolve, reject });
        socket!.send(JSON.stringify({ id, method, params, sessionId }));
      });
    const target = await command("Target.createTarget", { url: "about:blank" });
    const { sessionId } = await command("Target.attachToTarget", {
      targetId: target.targetId,
      flatten: true,
    });
    await command("Runtime.enable", {}, sessionId);
    await command("Page.navigate", { url }, sessionId);
    const until = Date.now() + 45000;
    let output = "";
    while (Date.now() < until) {
      const result = await command(
        "Runtime.evaluate",
        { expression: "document.body?.textContent ?? ''", returnByValue: true },
        sessionId,
      );
      output = result.result.value ?? "";
      if (output === pass) return;
      if (output.startsWith(fail))
        throw new Error(`${output}\nConsole: ${consoleMessages.slice(-20).join("\n")}`);
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    throw new Error(`Browser timed out: ${output}`);
  } finally {
    clearTimeout(deadline);
    socket?.close();
    browser?.kill();
    if (browser) await browser.exited;
    await rm(profile, { recursive: true, force: true });
  }
}
