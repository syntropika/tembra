/** Run a repository browser check in an isolated Chrome profile and always stop that process. */
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";

interface Reply {
  targetId?: string;
  sessionId?: string;
  result?: { value?: unknown };
}

/** Wait for the page's explicit success or failure marker through Chrome's existing debugger. */
export async function checkPage(url: string, marker: string): Promise<void> {
  const profile = await mkdtemp(
    path.join(process.env.TEMBRA_TEST_ARTIFACTS ?? tmpdir(), "tembra-rpc-chrome-"),
  );
  const browser = Bun.spawn(
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
  let socket: WebSocket | undefined;
  try {
    const reader = browser.stderr.getReader();
    let diagnostics = "";
    let endpoint: string | undefined;
    while (!endpoint) {
      const chunk = await reader.read();
      if (chunk.done) throw new Error(`Chrome exited before opening its debugger: ${diagnostics}`);
      diagnostics += new TextDecoder().decode(chunk.value);
      endpoint = diagnostics.match(/DevTools listening on (ws:\/\/\S+)/)?.[1];
    }
    reader.releaseLock();
    socket = new WebSocket(endpoint);
    await new Promise<void>((resolve, reject) => {
      socket!.onopen = () => resolve();
      socket!.onerror = reject;
    });
    let next = 0;
    const pending = new Map<
      number,
      { resolve(value: Reply): void; reject(error: unknown): void }
    >();
    socket.onmessage = (event) => {
      const reply = JSON.parse(String(event.data)) as {
        id?: number;
        error?: unknown;
        result?: Reply;
      };
      const request = reply.id && pending.get(reply.id);
      if (!request) return;
      pending.delete(reply.id!);
      if (reply.error) request.reject(new Error(JSON.stringify(reply.error)));
      else request.resolve(reply.result ?? {});
    };
    /** Send one debugger command without involving the product's RPC protocol. */
    const command = (method: string, params: unknown = {}, sessionId?: string): Promise<Reply> => {
      const id = ++next;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        socket!.send(JSON.stringify({ id, method, params, sessionId }));
      });
    };
    const target = await command("Target.createTarget", { url: "about:blank" });
    const attached = await command("Target.attachToTarget", {
      targetId: target.targetId,
      flatten: true,
    });
    await command("Page.navigate", { url }, attached.sessionId);
    const deadline = Date.now() + 30_000;
    while (Date.now() < deadline) {
      const reply = await command(
        "Runtime.evaluate",
        { expression: "document.body?.textContent ?? ''", returnByValue: true },
        attached.sessionId,
      );
      const output = String(reply.result?.value ?? "");
      if (output === `${marker}_PASS`) return;
      if (output.startsWith(`${marker}_FAIL`)) throw new Error(output);
      await Bun.sleep(25);
    }
    throw new Error("Browser RPC resource check timed out");
  } finally {
    socket?.close();
    browser.kill();
    await browser.exited;
    await rm(profile, { recursive: true, force: true });
  }
}
