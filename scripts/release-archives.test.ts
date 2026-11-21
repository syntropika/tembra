import { expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, truncateSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { checkArchiveSize, reusePublishedArchives } from "./release/archives.ts";

test("an oversized archive fails the release budget before publication", () => {
  const root = mkdtempSync(path.join(tmpdir(), "tembra-archive-size-"));
  try {
    const file = path.join(root, "mobile.tgz");
    writeFileSync(file, "");
    truncateSync(file, 100 * 1024 * 1024);
    expect(() => checkArchiveSize(file)).not.toThrow();
    truncateSync(file, 100 * 1024 * 1024 + 1);
    expect(() => checkArchiveSize(file)).toThrow("must not exceed 100 MiB");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("recovery retains published bytes and leaves the unpublished package rebuilt", async () => {
  const root = mkdtempSync(path.join(tmpdir(), "tembra-archive-reuse-"));
  const previous = path.join(root, "previous");
  const current = path.join(root, "current");
  const integrity = (value: string) =>
    `sha512-${createHash("sha512").update(value).digest("base64")}`;
  const names = ["engine-node", "react-native"];
  try {
    for (const [directory, content] of [
      [previous, "qualified"],
      [current, "rebuilt"],
    ]) {
      mkdirSync(directory!);
      const items = names.map((name) => ({
        name: `@syntropika/tembra-${name}`,
        version: "0.3.0",
        filename: `${name}.tgz`,
        integrity: integrity(content!),
      }));
      for (const item of items) writeFileSync(path.join(directory!, item.filename), content!);
      writeFileSync(path.join(directory!, "packages.json"), JSON.stringify(items));
    }
    const response = (status = 200, digest = integrity("qualified")) =>
      Object.assign(
        async (input: string | URL | Request) =>
          String(input).includes("react-native")
            ? new Response(null, { status: 404 })
            : status === 200
              ? Response.json({
                  name: "@syntropika/tembra-engine-node",
                  version: "0.3.0",
                  dist: { integrity: digest },
                })
              : new Response(null, { status }),
        { preconnect: fetch.preconnect },
      );
    const before = readFileSync(path.join(current, "packages.json"), "utf8");
    for (const request of [response(503), response(200, integrity("other"))]) {
      await expect(reusePublishedArchives(current, previous, request)).rejects.toThrow();
      expect(readFileSync(path.join(current, "engine-node.tgz"), "utf8")).toBe("rebuilt");
      expect(readFileSync(path.join(current, "packages.json"), "utf8")).toBe(before);
    }
    writeFileSync(path.join(previous, "react-native.tgz"), "tampered");
    await expect(reusePublishedArchives(current, previous, response())).rejects.toThrow(
      "integrity differs",
    );
    expect(readFileSync(path.join(current, "engine-node.tgz"), "utf8")).toBe("rebuilt");
    writeFileSync(path.join(previous, "react-native.tgz"), "qualified");
    await reusePublishedArchives(current, previous, response());
    expect(readFileSync(path.join(current, "engine-node.tgz"), "utf8")).toBe("qualified");
    expect(readFileSync(path.join(current, "react-native.tgz"), "utf8")).toBe("rebuilt");
    const manifest = JSON.parse(readFileSync(path.join(current, "packages.json"), "utf8"));
    expect(manifest.map((item: { integrity: string }) => item.integrity)).toEqual([
      integrity("qualified"),
      integrity("rebuilt"),
    ]);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
