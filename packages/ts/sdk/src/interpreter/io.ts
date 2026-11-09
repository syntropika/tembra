import { boundedText, type FilesystemProvider, type MountedFilesystem } from "./filesystem.ts";
import type { IORequest } from "./host-protocol.ts";
import type { Grants } from "../authoring/types.ts";

/** Installed mounts remain host capabilities; the guest supplies only an operation and path. */
export class BrowserCapabilities {
  readonly #mounts: { path: string; writable: boolean; filesystem: MountedFilesystem }[] = [];

  private constructor() {}

  static async open(
    grants: Grants,
    providers: Readonly<Record<string, FilesystemProvider>>,
  ): Promise<BrowserCapabilities> {
    const capabilities = new BrowserCapabilities();
    try {
      for (const grant of grants.filesystem ?? []) {
        const provider = providers[grant.provider];
        if (!provider) throw new Error(`Unknown filesystem provider ${grant.provider}`);
        capabilities.#mounts.push({
          path: grant.mount,
          writable: grant.writable,
          filesystem: provider.open(grant.mount, grant.writable),
        });
      }
      capabilities.#mounts.sort((a, b) => b.path.length - a.path.length);
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

  /** Authority and liveness are checked by Scope immediately before this operation is admitted. */
  async execute(request: IORequest): Promise<unknown> {
    if (
      request.operation !== "filesystem.read-text" &&
      request.operation !== "filesystem.write-text"
    )
      throw new Error("This browser host has no raw TCP provider");
    const path = request.path;
    if (
      !path.startsWith("/") ||
      path.includes("\\") ||
      path.includes("\0") ||
      path
        .split("/")
        .slice(1)
        .some((part) => !part || part === "." || part === "..")
    )
      throw new Error("Filesystem paths must be normalized absolute paths");
    for (const mount of this.#mounts) {
      if (!path.startsWith(`${mount.path}/`)) continue;
      const relative = path.slice(mount.path.length + 1);
      if (request.operation === "filesystem.read-text")
        return boundedText(await mount.filesystem.readText(relative));
      if (!mount.writable) throw new Error("Filesystem mount is read-only");
      await mount.filesystem.writeText(relative, boundedText(request.value));
      return null;
    }
    throw new Error("Filesystem path is outside the granted mounts");
  }

  /** Called after the transport has awaited every physically admitted host operation. */
  async dispose(): Promise<void> {
    const mounts = this.#mounts.splice(0);
    const settled = await Promise.allSettled(
      mounts.map(async (mount) => mount.filesystem.dispose()),
    );
    const errors = settled
      .filter((result) => result.status === "rejected")
      .map((result) => result.reason);
    if (errors.length) throw new AggregateError(errors, "Filesystem disposal failed");
  }
}
