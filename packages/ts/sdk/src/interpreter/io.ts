import { BrowserNetwork, type BrowserNetworkProviders } from "./browser-network.ts";
import type { FilesystemProvider } from "./filesystem.ts";
import type { StandardMount } from "./component/wasi-filesystem.ts";
import type { Grants } from "../authoring/types.ts";

/**
 * Installed grants remain host capabilities. Filesystem mounts, HTTP and sockets are reached
 * only through the standard WASI host; there is no other host I/O channel.
 */
export class BrowserCapabilities {
  readonly #mounts: StandardMount[] = [];
  #preopens: readonly StandardMount[] = [];

  private constructor(readonly network: BrowserNetwork) {}

  static async open(
    grants: Grants,
    providers: BrowserNetworkProviders & {
      filesystem?: Readonly<Record<string, FilesystemProvider>>;
    },
  ): Promise<BrowserCapabilities> {
    const capabilities = new BrowserCapabilities(new BrowserNetwork(grants, providers));
    try {
      for (const grant of grants.filesystem ?? []) {
        const provider = providers.filesystem?.[grant.provider];
        if (!provider) throw new Error(`Unknown filesystem provider ${grant.provider}`);
        capabilities.#mounts.push({
          path: grant.mount,
          writable: grant.writable,
          filesystem: provider.open(grant.mount, grant.writable),
        });
      }
      capabilities.#preopens = Object.freeze([...capabilities.#mounts]);
    } catch (error) {
      try {
        await capabilities.dispose();
      } catch (disposal) {
        throw new AggregateError(
          [error, disposal],
          "Filesystem initialization and disposal failed",
        );
      }
      throw error;
    }
    return capabilities;
  }

  /** Mounts in grant order for standard `wasi:filesystem` preopens. */
  get mounts(): readonly StandardMount[] {
    return this.#preopens;
  }

  /** Called after the transport has awaited every physically admitted host operation. */
  async dispose(): Promise<void> {
    const mounts = this.#mounts.splice(0);
    this.#preopens = [];
    const settled = await Promise.allSettled([
      Promise.resolve().then(() => this.network.dispose()),
      ...mounts.map(async (mount) => mount.filesystem.dispose()),
    ]);
    const errors = settled
      .filter((result) => result.status === "rejected")
      .map((result) => result.reason);
    if (errors.length) throw new AggregateError(errors, "Filesystem disposal failed");
  }
}
