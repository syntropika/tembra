import type { EsmArtifactV1, ReloadReport } from "@tembra/sdk";

/** Portable delivery record; supplied artifacts still require the executor's normal validation. */
export type DevelopmentArtifact = {
  /** Envelope submitted to the existing browser executor. */
  readonly artifact: EsmArtifactV1;
  /** Named ESM export to install. */
  readonly selectedExport: string;
  /** Stable configured build target name. */
  readonly target: string;
};
/** Active browser polling connection; the caller owns closing it before disposing its executor. */
export type BrowserDevelopmentConnection = {
  /** Resolves after the first successful poll, or after close if no poll succeeded. */
  readonly ready: Promise<void>;
  /** Stop polls, abort fetches/backoff and join the active apply callback; safe to repeat. */
  close: () => Promise<void>;
};
type Update = { readonly id: number; readonly artifacts: readonly DevelopmentArtifact[] };
type Failure = {
  readonly code?: string;
  readonly reason?: string;
  readonly message: string;
  readonly details?: unknown;
};
type Reply =
  | { readonly id: number; readonly ok: true; readonly report?: ReloadReport }
  | { readonly id: number; readonly ok: false; readonly error: Failure };
function artifactValue(value: unknown): value is DevelopmentArtifact {
  if (
    value === null ||
    typeof value !== "object" ||
    !("target" in value) ||
    typeof value.target !== "string" ||
    !("selectedExport" in value) ||
    typeof value.selectedExport !== "string" ||
    !("artifact" in value)
  ) {
    return false;
  }
  const { artifact } = value;
  return (
    artifact !== null &&
    typeof artifact === "object" &&
    "format" in artifact &&
    artifact.format === "tembra-esm-v1" &&
    "identity" in artifact &&
    typeof artifact.identity === "string" &&
    "source" in artifact &&
    typeof artifact.source === "string" &&
    "sha256" in artifact &&
    typeof artifact.sha256 === "string" &&
    "exports" in artifact &&
    Array.isArray(artifact.exports) &&
    artifact.exports.every((name: unknown) => typeof name === "string")
  );
}
function decodeUpdate(value: unknown): Update {
  if (
    value === null ||
    typeof value !== "object" ||
    !("id" in value) ||
    typeof value.id !== "number" ||
    !Number.isSafeInteger(value.id) ||
    !("artifacts" in value) ||
    !Array.isArray(value.artifacts) ||
    !value.artifacts.every(artifactValue)
  ) {
    throw new TypeError("Invalid development update");
  }
  return { id: value.id, artifacts: value.artifacts };
}
function failureValue(error: unknown): Failure {
  const value = error !== null && typeof error === "object" ? error : {};
  return {
    ...("code" in value && typeof value.code === "string" ? { code: value.code } : {}),
    ...("reason" in value && typeof value.reason === "string" ? { reason: value.reason } : {}),
    message:
      "message" in value && typeof value.message === "string" ? value.message : String(error),
    ...("details" in value && value.details !== undefined ? { details: value.details } : {}),
  };
}
/**
 * Connect an explicitly authorized loopback endpoint to the embedding's existing executor.
 * Browser-only entrypoint: `@tembra/tooling/browser` has no Node imports. Captures connection settings
 * and callbacks synchronously; invalid URLs throw before polling. Poll errors reach `onError` and
 * retry after 50 milliseconds. Readiness is connectivity, not successful facet installation.
 * Acknowledgement follows the supplied apply callback's settlement, including structured failures.
 * Each connection has its own delivery cursor; another tab does not replay this connection's work.
 * A failed acknowledgement request is retried without calling apply again for that update.
 * If the bridge has expired this connection's acknowledgement state, onError receives a diagnostic
 * and polling stops; the update may already be installed, so reconnection is an explicit caller choice.
 * Close stops new polls, requests cancellation, and joins the active apply; callbacks must settle
 * their owned work. Cancellation does not undo installation or external effects already performed.
 * The embedding chooses initial `load` versus later `reloadArtifacts`; the tooling guide's
 * explicit development installation section describes that boundary.
 *
 * @example
 * ```ts
 * import type { BrowserExecutor } from "@tembra/sdk/browser";
 * import { connectDevelopmentHost } from "@tembra/tooling/browser";
 * // The executor already contains the facets managed by this connection.
 * export async function watchInstalledFacets(
 *   executor: BrowserExecutor, url: string, token: string, until: Promise<void>,
 * ): Promise<void> {
 *   const connection = connectDevelopmentHost({
 *     url, token,
 *     apply: updates => executor.reloadArtifacts(updates.map(update => ({
 *       artifact: update.artifact, selected: update.selectedExport,
 *     }))),
 *     onError: console.error,
 *   });
 *   try {
 *     await connection.ready;
 *     await until;
 *   } finally {
 *     await connection.close();
 *   }
 * }
 * ```
 */
export function connectDevelopmentHost(options: {
  /** Bridge origin; only HTTP on localhost, 127.0.0.1 or ::1 is accepted. */
  readonly url: string;
  /** Bearer secret issued by the matching `createBrowserBridge`. */
  readonly token: string;
  /** Awaited serially before acknowledging; must settle its work after cancellation. */
  readonly apply: (
    artifacts: readonly DevelopmentArtifact[],
    signal: AbortSignal,
  ) => Promise<ReloadReport | undefined> | Promise<void>;
  /** Synchronous failure observer; exceptions are ignored and returned promises are not joined. */
  readonly onError?: (error: Error) => void;
}): BrowserDevelopmentConnection {
  const { apply: applyUpdate, onError } = options;
  const url = new URL(options.url);
  if (url.protocol !== "http:" || !["127.0.0.1", "localhost", "[::1]"].includes(url.hostname)) {
    throw new Error("Development endpoints must use loopback HTTP");
  }
  const controller = new AbortController();
  const client = crypto.randomUUID();
  let markReady!: () => void;
  // oxlint-disable-next-line promise/avoid-new -- Readiness belongs to the first completed network poll.
  const ready = new Promise<void>((resolve) => {
    markReady = resolve;
  });
  const headers = { Authorization: `Bearer ${options.token}`, "Content-Type": "application/json" };
  let awaitingAck: Reply | undefined;
  let acknowledged = 0;
  function report(error: unknown): void {
    if (controller.signal.aborted) {
      return;
    }
    try {
      onError?.(
        error instanceof Error ? error : new Error("Development apply failed", { cause: error }),
      );
    } catch {
      /* Observer exceptions do not control connection ownership. */
    }
  }
  function expiredCursor(): void {
    report(
      new Error(
        "The bridge expired or lost this client's acknowledged cursor; an update may already be installed. Reconnect explicitly.",
      ),
    );
    controller.abort();
  }
  async function delay(): Promise<void> {
    // oxlint-disable-next-line promise/avoid-new -- The timer and AbortSignal jointly own this cancellable polling delay.
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        controller.signal.removeEventListener("abort", abort);
        resolve();
      }, 50);
      function abort(): void {
        clearTimeout(timer);
        controller.signal.removeEventListener("abort", abort);
        resolve();
      }
      controller.signal.addEventListener("abort", abort, { once: true });
      if (controller.signal.aborted) {
        abort();
      }
    });
  }
  async function apply(update: Update): Promise<Reply> {
    try {
      const result = await applyUpdate(update.artifacts, controller.signal);
      return result === undefined
        ? { id: update.id, ok: true }
        : { id: update.id, ok: true, report: result };
    } catch (error) {
      report(error);
      return { id: update.id, ok: false, error: failureValue(error) };
    }
  }
  async function poll(): Promise<void> {
    if (awaitingAck === undefined) {
      const response = await fetch(
        new URL(`/updates?client=${encodeURIComponent(client)}&cursor=${acknowledged}`, url),
        {
          headers,
          signal: controller.signal,
        },
      );
      if (response.status === 409) {
        expiredCursor();
        return;
      }
      if (!response.ok) {
        throw new Error(`Development poll failed with HTTP ${response.status}`);
      }
      markReady();
      if (response.status === 204) {
        return;
      }
      const value: unknown = await response.json();
      awaitingAck = await apply(decodeUpdate(value));
    }
    if (controller.signal.aborted) {
      return;
    }
    const acknowledgement = await fetch(new URL(`/ack?client=${encodeURIComponent(client)}`, url), {
      method: "POST",
      headers,
      body: JSON.stringify(awaitingAck),
      signal: controller.signal,
    });
    if (acknowledgement.status === 409) {
      expiredCursor();
      return;
    }
    if (!acknowledgement.ok) {
      throw new Error(`Development acknowledgement failed with HTTP ${acknowledgement.status}`);
    }
    acknowledged = awaitingAck.id;
    awaitingAck = undefined;
  }
  const running = (async () => {
    while (!controller.signal.aborted) {
      try {
        // oxlint-disable-next-line no-await-in-loop -- Settle each apply before another poll admits a replacement.
        await poll();
      } catch (error) {
        report(error);
      }
      if (!controller.signal.aborted) {
        // oxlint-disable-next-line no-await-in-loop -- Backoff belongs to the completed poll and is cancelled by close.
        await delay();
      }
    }
  })().finally(markReady);
  return {
    ready,
    async close() {
      controller.abort();
      await running;
    },
  };
}
