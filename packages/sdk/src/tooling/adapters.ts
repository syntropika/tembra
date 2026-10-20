import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { createServer } from "node:http";
import type { IncomingMessage, ServerResponse } from "node:http";

import type { ReloadReport } from "@tembra/sdk";

import type { BuiltArtifact, DevelopmentHost, ReloadOutcome } from "./types.js";

const send = (response: ServerResponse, status: number, body?: unknown) => {
  response.writeHead(status, {
    "Content-Type": "application/json",
    "Cache-Control": "no-store",
    "Access-Control-Allow-Origin": "*",
  });
  response.end(body === undefined ? undefined : JSON.stringify(body));
};

/** Portable delivery record consumed by an embedding's existing executor. */
export type DevelopmentArtifact = {
  /** Validated envelope; the executor still enforces its installation and evaluation limits. */
  readonly artifact: BuiltArtifact["artifact"];
  /** Named ESM export selected for installation. */
  readonly selectedExport: string;
  /** Configured build target name; stable across replacement deliveries. */
  readonly target: string;
};
type Failure = {
  readonly code?: string;
  readonly reason?: string;
  readonly message: string;
  readonly details?: unknown;
};
type Reply =
  | { readonly id: number; readonly ok: true; readonly report?: ReloadReport }
  | { readonly id: number; readonly ok: false; readonly error: Failure };

/** A completed remote refusal, retaining the embedding's structured diagnostic fields. */
export class HostDeliveryError extends Error {
  /** Original embedding error code, when supplied. */
  readonly code: string | undefined;
  /** Original embedding reason, including `rotation_required`, when supplied. */
  readonly reason: string | undefined;
  /** Original decoded diagnostic value; retained by reference when constructed directly. */
  readonly details: unknown;
  constructor(failure: Failure) {
    super(failure.message);
    this.name = "HostDeliveryError";
    this.code = failure.code;
    this.reason = failure.reason;
    this.details = failure.details;
  }
}
function wire(artifacts: readonly BuiltArtifact[]): DevelopmentArtifact[] {
  return artifacts.map(({ artifact, selectedExport, target }) => ({
    artifact,
    selectedExport,
    target,
  }));
}
function cancelled(): Error {
  return new DOMException("Development delivery was cancelled", "AbortError");
}
function stringList(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item: unknown) => typeof item === "string");
}
function numberList(value: unknown): value is number[] {
  return Array.isArray(value) && value.every((item: unknown) => typeof item === "number");
}
function decodeReport(value: unknown): ReloadReport {
  if (
    value === null ||
    typeof value !== "object" ||
    !("codeVersion" in value) ||
    typeof value.codeVersion !== "number" ||
    !("changed" in value) ||
    !stringList(value.changed) ||
    !("removed" in value) ||
    !stringList(value.removed) ||
    !("preparedObjects" in value) ||
    !numberList(value.preparedObjects) ||
    !("gatedObjects" in value) ||
    !numberList(value.gatedObjects) ||
    !("lingeringPhysical" in value) ||
    typeof value.lingeringPhysical !== "number"
  ) {
    throw new TypeError("Invalid development reload report");
  }
  const previous = "previousVersion" in value ? value.previousVersion : undefined;
  if (previous !== null && previous !== undefined && typeof previous !== "number") {
    throw new TypeError("Invalid previous development code version");
  }
  return {
    codeVersion: value.codeVersion,
    previousVersion: previous ?? undefined,
    changed: value.changed,
    removed: value.removed,
    preparedObjects: value.preparedObjects,
    gatedObjects: value.gatedObjects,
    lingeringPhysical: value.lingeringPhysical,
  };
}
function decodeFailure(value: unknown): Failure {
  if (
    value === null ||
    typeof value !== "object" ||
    !("message" in value) ||
    typeof value.message !== "string"
  ) {
    throw new TypeError("Invalid development failure");
  }
  return {
    message: value.message,
    ...("code" in value && typeof value.code === "string" ? { code: value.code } : {}),
    ...("reason" in value && typeof value.reason === "string" ? { reason: value.reason } : {}),
    ...("details" in value ? { details: value.details } : {}),
  };
}
function decodeReply(value: unknown): Reply {
  if (
    value === null ||
    typeof value !== "object" ||
    !("id" in value) ||
    typeof value.id !== "number" ||
    !Number.isSafeInteger(value.id) ||
    !("ok" in value) ||
    typeof value.ok !== "boolean"
  ) {
    throw new TypeError("Invalid development acknowledgement");
  }
  if (value.ok) {
    return "report" in value && value.report !== undefined
      ? { id: value.id, ok: true, report: decodeReport(value.report) }
      : { id: value.id, ok: true };
  }
  if (!("error" in value)) {
    throw new TypeError("Development refusal has no error");
  }
  return { id: value.id, ok: false, error: decodeFailure(value.error) };
}

function outcome(reply: Reply): ReloadOutcome {
  if (!reply.ok) {
    throw new HostDeliveryError(reply.error);
  }
  return reply.report === undefined
    ? { status: "applied" }
    : { status: "applied", report: reply.report };
}
type Pending = { finish: (reply: Reply) => void; fail: (error: Error) => void };
async function acknowledgement(
  pending: Map<number, Pending>,
  id: number,
  signal: AbortSignal,
  timeout: number,
): Promise<ReloadOutcome> {
  // oxlint-disable-next-line promise/avoid-new -- Acknowledgement, timeout and cancellation jointly own one pending transport request.
  return await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      reject(new Error("Development acknowledgement timed out"));
    }, timeout);
    function cleanup(): void {
      clearTimeout(timer);
      signal.removeEventListener("abort", abort);
      pending.delete(id);
    }
    function abort(): void {
      cleanup();
      reject(cancelled());
    }

    pending.set(id, {
      finish(reply) {
        cleanup();
        try {
          resolve(outcome(reply));
        } catch (error) {
          reject(
            error instanceof Error
              ? error
              : new Error("Development acknowledgement failed", { cause: error }),
          );
        }
      },
      fail(error) {
        cleanup();
        reject(error);
      },
    });
    signal.addEventListener("abort", abort, { once: true });
    if (signal.aborted) {
      abort();
    }
  });
}

/**
 * Spawn an external embedding that accepts development artifacts through a JSON-line protocol.
 * Owns one child process and, where supported, its process group. The child receives complete
 * artifacts on stdin and must acknowledge actual install/reload on stdout; stderr is inherited.
 * Captures command and observer options synchronously. Spawn failures reach the next `apply`;
 * malformed replies or a thrown event observer fail pending and subsequent deliveries.
 * Abort and acknowledgement timeout stop waiting; the remote operation may already have committed.
 * Close ends stdin, requests termination after one second, forces termination after two, and joins exit.
 * Graceful close continues forwarding final child events while rejecting new and pending deliveries.
 * Platforms without process-group signalling can guarantee cleanup of the immediate child only.
 */
export function createStdioHost(options: {
  /** Executable followed by literal arguments; no shell expansion. Empty argv throws synchronously. */
  readonly command: readonly string[];
  /** Child working directory; defaults to the caller's current directory. */
  readonly cwd?: string;
  /** Milliseconds to await a reply, default 10_000. Timeout leaves remote commit status unknown. */
  readonly ackTimeout?: number;
  /** Synchronous notification for JSON lines without an id; returned promises are not joined. */
  readonly onEvent?: (event: unknown) => void;
}): DevelopmentHost {
  const { ackTimeout, onEvent } = options;
  const [command, ...args] = options.command;
  if (command === undefined || command.length === 0) {
    throw new Error("A development host command is required");
  }
  const grouped = process.platform !== "win32";
  const child = spawn(command, args, {
    ...(options.cwd === undefined ? {} : { cwd: options.cwd }),
    stdio: ["pipe", "pipe", "inherit"],
    detached: grouped,
  });
  const kill = (signal: NodeJS.Signals) => {
    if (grouped && child.pid !== undefined) {
      try {
        process.kill(-child.pid, signal);
      } catch {
        /* The owned process group already exited. */
      }
    } else {
      child.kill(signal);
    }
  };
  const pending = new Map<number, Pending>();
  let nextId = 0;
  let closed = false;
  let failure: Error | undefined;
  let buffer = "";
  const rejectPending = (error: Error) => {
    for (const request of pending.values()) {
      request.fail(error);
    }
  };
  const fail = (error: Error) => {
    failure ??= error;
    rejectPending(failure);
  };
  child.on("error", fail);
  child.stdin.on("error", fail);
  // oxlint-disable-next-line promise/avoid-new -- Child close joins process termination and its stdio before resource teardown completes.
  const exited = new Promise<void>((resolve) => {
    child.once("close", () => {
      fail(new Error("Development host process exited"));
      resolve();
    });
  });
  child.stdout.setEncoding("utf-8");
  child.stdout.on("data", (chunk: string) => {
    if (failure !== undefined) {
      return;
    }
    buffer += chunk;
    if (buffer.length > 2 * 1024 * 1024) {
      fail(new Error("Development acknowledgement exceeds the size limit"));
      child.kill();
      return;
    }
    let newline: number;
    while ((newline = buffer.indexOf("\n")) !== -1) {
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      if (!line.trim()) {
        continue;
      }
      try {
        const value: unknown = JSON.parse(line);
        if (value !== null && typeof value === "object" && "id" in value) {
          const reply = decodeReply(value);
          pending.get(reply.id)?.finish(reply);
        } else {
          onEvent?.(value);
        }
      } catch (error) {
        fail(error instanceof Error ? error : new Error(String(error)));
      }
    }
  });
  let closing: Promise<void> | undefined;
  return {
    async apply(artifacts, signal) {
      if (closed || signal.aborted) {
        throw cancelled();
      }
      if (failure) {
        throw failure;
      }
      nextId += 1;
      const id = nextId;
      const result = acknowledgement(pending, id, signal, ackTimeout ?? 10_000);
      if (!signal.aborted) {
        child.stdin.write(`${JSON.stringify({ id, artifacts: wire(artifacts) })}\n`);
      }
      return await result;
    },
    async close() {
      await (closing ??= (async () => {
        closed = true;
        rejectPending(cancelled());
        child.stdin.end();
        const terminate = setTimeout(() => {
          kill("SIGTERM");
        }, 1000);
        const force = setTimeout(() => {
          kill("SIGKILL");
        }, 2000);
        try {
          await exited;
        } finally {
          clearTimeout(terminate);
          clearTimeout(force);
          // Descendants with detached stdio must not outlive the parent-owned host.
          if (grouped) {
            kill("SIGKILL");
          }
        }
      })());
    },
  };
}

/** Listening Bun capability ready to pass to `dev` and an explicitly authorized browser client. */
export type BrowserDevelopmentHost = {
  /** Loopback HTTP origin with the bound port; valid until close. */
  readonly url: string;
  /** Bearer secret to share only with the intended development client. */
  readonly token: string;
  /** Stop deliveries, reject pending acknowledgements and join listener cleanup; idempotent. */
  close: () => Promise<void>;
} & DevelopmentHost;
/**
 * Listen on 127.0.0.1 and issue a browser installation capability; resolves after the port is bound.
 * Captures option values and artifact delivery values before yielding. Pass the host to `dev` to
 * transfer cleanup ownership, or call its idempotent `close` directly when managing it yourself.
 * Each browser has an independent offer and acknowledgement cursor. An apply waits for every client
 * that polled in the preceding two seconds; later arrivals receive the latest retained snapshot.
 * Applied therefore acknowledges those captured clients, not every future browser. With multiple
 * clients no single host's reload report is returned. Calls serialize, and empty batches do no work.
 * A refusal restores retained replay values, but cannot undo another browser's successful update.
 * A timeout or cancellation leaves remote outcomes unknown. Client records expire after thirty
 * seconds without traffic once local waits settle; at most 128 records are retained (HTTP 429
 * rejects excess clients). Expired clients presenting a nonzero acknowledged cursor receive HTTP 409
 * rather than replaying previously processed code; new clients still receive retained snapshots.
 * Closing joins the listener and local deliveries;
 * it cannot establish that an operation already admitted by a remote browser physically stopped.
 * Invalid custom tokens reject with `TypeError` before listener acquisition. Bind failures reject
 * with the original server error; explicit remote refusals are `HostDeliveryError`.
 *
 * @example
 * ```ts
 * import { once } from "node:events";
 * import { createBrowserBridge, dev, loadConfig } from "@tembra/sdk/tooling";
 * const config = await loadConfig();
 * const browser = await createBrowserBridge();
 * try {
 *   console.log(browser.url, browser.token); // Give these to the authorized local client.
 *   const session = await dev(config, { hosts: { browser }, onError: console.error });
 *   try {
 *     await session.ready;
 *     await once(process, "SIGINT");
 *   } finally {
 *     await session.close();
 *   }
 * } finally {
 *   await browser.close();
 * }
 * ```
 */
export async function createBrowserBridge(
  options: {
    /** TCP port; omitted or zero asks the operating system for an available port. */
    readonly port?: number;
    /**
     * Bearer secret of at least 16 printable ASCII characters (0x21–0x7e), without whitespace
     * or controls. Defaults to 32 random bytes in hex. Invalid values reject with `TypeError`
     * before listener acquisition.
     */
    readonly token?: string;
    /** Synchronous late-refusal observer; exceptions are ignored and returned promises not joined. */
    readonly onError?: (error: Error) => void;
    /** Milliseconds to await each captured active client's acknowledgement, default 10_000. */
    readonly ackTimeout?: number;
  } = {},
): Promise<BrowserDevelopmentHost> {
  const { ackTimeout, onError } = options;
  const token = options.token ?? randomBytes(32).toString("hex");
  if (typeof token !== "string" || token.length < 16 || /[^\u0021-\u007E]/u.test(token)) {
    throw new TypeError(
      "The browser capability token must contain at least 16 printable ASCII characters without whitespace",
    );
  }
  const snapshots = new Map<string, { value: DevelopmentArtifact; version: number }>();
  const accepted = new Map<string, DevelopmentArtifact>();
  type Offered = {
    readonly id: number;
    readonly entries: readonly { value: DevelopmentArtifact; version: number }[];
  };
  type Client = {
    lastPoll: number;
    acknowledged: number;
    offered?: Offered;
    readonly pending: Map<number, Pending>;
  };
  const clients = new Map<string, Client>();
  let version = 0;
  let closed = false;
  let deliveries = Promise.resolve();
  const requests = new Set<Promise<void>>();

  function expireClients(now: number): void {
    for (const [identity, state] of clients) {
      if (now - state.lastPoll > 30_000 && state.pending.size === 0) {
        clients.delete(identity);
      }
    }
  }
  function restore(entries: Offered["entries"], id: number): void {
    for (const { value } of entries) {
      const current = snapshots.get(value.target);
      if (current !== undefined && current.version <= id) {
        const previous = accepted.get(value.target);
        if (previous === undefined) {
          snapshots.delete(value.target);
        } else {
          snapshots.set(value.target, { value: previous, version: current.version });
        }
      }
    }
  }
  function pollUpdate(url: URL, response: ServerResponse): void {
    const identity = url.searchParams.get("client");
    if (identity === null || identity.length === 0 || identity.length > 128) {
      send(response, 400);
      return;
    }
    const cursorText = url.searchParams.get("cursor");
    const cursor = cursorText === null ? 0 : Number(cursorText);
    if (
      url.searchParams.getAll("cursor").length > 1 ||
      (cursorText !== null && !/^(?:0|[1-9]\d*)$/u.test(cursorText)) ||
      !Number.isSafeInteger(cursor)
    ) {
      send(response, 400);
      return;
    }
    const now = Date.now();
    expireClients(now);
    let state = clients.get(identity);
    if (state === undefined) {
      if (cursor !== 0) {
        send(response, 409, {
          message: "The browser acknowledgement cursor expired; reconnect explicitly",
        });
        return;
      }
      if (clients.size >= 128) {
        send(response, 429, { message: "The browser development client limit was reached" });
        return;
      }
      state = { lastPoll: now, acknowledged: 0, pending: new Map() };
      clients.set(identity, state);
    }
    state.lastPoll = now;
    if (state.offered !== undefined) {
      send(response, 200, {
        id: state.offered.id,
        artifacts: state.offered.entries.map((entry) => entry.value),
      });
      return;
    }
    const entries = [...snapshots.values()].filter((entry) => entry.version > state.acknowledged);
    if (entries.length === 0) {
      send(response, 204);
      return;
    }
    state.offered = { id: version, entries };
    send(response, 200, { id: version, artifacts: entries.map((entry) => entry.value) });
  }
  async function acknowledge(
    request: IncomingMessage,
    url: URL,
    response: ServerResponse,
  ): Promise<void> {
    const identity = url.searchParams.get("client");
    const state = identity === null ? undefined : clients.get(identity);
    if (state === undefined) {
      send(response, 409);
      return;
    }
    let body = "";
    try {
      for await (const chunk of request) {
        body += String(chunk);
        if (body.length > 64 * 1024) {
          send(response, 413);
          return;
        }
      }
      const reply = decodeReply(JSON.parse(body));
      state.lastPoll = Date.now();
      const { offered } = state;
      // A retried acknowledgement must not install the same update again after a lost response.
      if (reply.id > 0 && reply.id === state.acknowledged) {
        send(response, 204);
        return;
      }
      if (offered === undefined || reply.id !== offered.id) {
        send(response, 409);
        return;
      }
      delete state.offered;
      state.acknowledged = Math.max(state.acknowledged, reply.id);
      const waiting = state.pending.get(reply.id);
      if (waiting !== undefined) {
        waiting.finish(reply);
      } else if (reply.ok) {
        for (const entry of offered.entries) {
          if (snapshots.get(entry.value.target)?.version === entry.version) {
            accepted.set(entry.value.target, entry.value);
          }
        }
      } else {
        restore(offered.entries, reply.id);
        try {
          onError?.(new HostDeliveryError(reply.error));
        } catch {
          /* Error observers cannot change acknowledgement handling. */
        }
      }
      send(response, 204);
    } catch {
      send(response, 400);
    }
  }
  async function deliver(
    captured: readonly DevelopmentArtifact[],
    signal: AbortSignal,
  ): Promise<ReloadOutcome> {
    if (closed || signal.aborted) {
      throw cancelled();
    }
    if (captured.length === 0) {
      return { status: "applied" };
    }
    const now = Date.now();
    expireClients(now);
    const intended = [...clients.values()].filter((state) => now - state.lastPoll <= 2000);
    version += 1;
    const id = version;
    const entries = captured.map((value) => ({ value, version: id }));
    for (const entry of entries) {
      snapshots.set(entry.value.target, entry);
    }
    if (intended.length === 0) {
      return { status: "pending", message: "Waiting for a browser development client" };
    }
    const results = await Promise.allSettled(
      intended.map(
        async (state) => await acknowledgement(state.pending, id, signal, ackTimeout ?? 10_000),
      ),
    );
    const failures = results.filter((result) => result.status === "rejected");
    if (failures.some((result) => result.reason instanceof HostDeliveryError)) {
      // This changes replay data only. Other browsers may already have committed the refused update.
      restore(entries, id);
    }
    const unknown = failures.find((result) => !(result.reason instanceof HostDeliveryError));
    const failure = unknown ?? failures[0];
    if (failure !== undefined) {
      const reason: unknown = failure.reason;
      throw reason instanceof Error
        ? reason
        : new Error("Browser delivery failed", { cause: reason });
    }
    for (const artifact of captured) {
      accepted.set(artifact.target, artifact);
    }
    const [only] = results;
    return results.length === 1 && only?.status === "fulfilled"
      ? only.value
      : { status: "applied" };
  }
  const handler = async (request: IncomingMessage, response: ServerResponse) => {
    // Preflight grants no capability; every useful request still requires the secret bearer token.
    if (request.method === "OPTIONS") {
      response.writeHead(204, {
        "Access-Control-Allow-Origin": "*",
        "Access-Control-Allow-Headers": "Authorization, Content-Type",
        "Access-Control-Allow-Methods": "GET, POST",
        "Access-Control-Allow-Private-Network": "true",
      });
      response.end();
      return;
    }
    if (request.headers.authorization !== `Bearer ${token}`) {
      send(response, 401);
      return;
    }
    if (closed) {
      send(response, 503);
      return;
    }
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    if (request.method === "GET" && url.pathname === "/updates") {
      pollUpdate(url, response);
      return;
    }
    if (request.method === "POST" && url.pathname === "/ack") {
      await acknowledge(request, url, response);
      return;
    }
    send(response, 404);
  };
  const server = createServer((request, response) => {
    const running = handler(request, response)
      .catch(() => {
        if (response.headersSent) {
          response.destroy();
        } else {
          send(response, 500);
        }
      })
      .finally(() => {
        requests.delete(running);
      });
    requests.add(running);
  });
  // oxlint-disable-next-line promise/avoid-new -- The server reports listener readiness and bind failure through distinct events.
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(options.port ?? 0, "127.0.0.1", () => {
      server.removeListener("error", reject);
      resolve();
    });
  });
  const address = server.address();
  if (address === null || typeof address === "string") {
    throw new Error("The browser bridge has no TCP address");
  }
  let closing: Promise<void> | undefined;
  return {
    url: `http://127.0.0.1:${address.port}`,
    token,
    async apply(artifacts, signal) {
      if (closed || signal.aborted) {
        throw cancelled();
      }
      const captured = structuredClone(wire(artifacts));
      const running = deliveries.then(async () => await deliver(captured, signal));
      deliveries = running.then(
        () => undefined,
        () => undefined,
      );
      return await running;
    },
    async close() {
      await (closing ??= (async () => {
        closed = true;
        for (const state of clients.values()) {
          for (const request of state.pending.values()) {
            request.fail(cancelled());
          }
        }
        // oxlint-disable-next-line promise/avoid-new -- Server close reports completion after all owned connections are closed.
        await new Promise<void>((resolve, reject) => {
          server.close((error) => {
            if (error === undefined) {
              resolve();
            } else {
              reject(error);
            }
          });
          server.closeAllConnections();
        });
        await Promise.allSettled([deliveries, ...requests]);
        clients.clear();
      })());
    },
  };
}
