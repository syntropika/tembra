import { expect, test } from "bun:test";
import { memoryFilesystem, type FilesystemProvider } from "../src/interpreter/filesystem.ts";
import { BrowserCapabilities } from "../src/interpreter/io.ts";

test("only granted mounts are opened with their exact rights and lifetime", async () => {
  const opened: [string, boolean][] = [];
  const provider = memoryFilesystem({ "input.txt": "value" });
  const tracked: FilesystemProvider = {
    open(path, writable) {
      opened.push([path, writable]);
      return provider.open(path, writable);
    },
  };
  const io = await BrowserCapabilities.open(
    {
      filesystem: [
        { provider: "data", mount: "/data", writable: true },
        { provider: "data", mount: "/data/nested", writable: false },
      ],
    },
    { filesystem: { data: tracked, unused: tracked } },
  );
  expect(opened).toEqual([
    ["/data", true],
    ["/data/nested", false],
  ]);
  expect(io.mounts.map(({ path, writable }) => [path, writable])).toEqual(opened);
  expect(Object.isFrozen(io.mounts)).toBe(true);
  const directory = io.mounts[0]!.filesystem.openRoot();
  await io.dispose();
  expect(io.mounts).toEqual([]);
  await expect(directory.stat()).rejects.toThrow();
  await io.dispose();
});

test("a later mount failure closes every already opened mount", async () => {
  let disposed = 0;
  const provider: FilesystemProvider = {
    open(path, writable) {
      const mount = memoryFilesystem().open(path, writable);
      return {
        writable: mount.writable,
        openRoot: () => mount.openRoot(),
        async dispose() {
          await mount.dispose();
          disposed++;
        },
      };
    },
  };
  await expect(
    BrowserCapabilities.open(
      {
        filesystem: [
          { provider: "data", mount: "/data", writable: false },
          { provider: "missing", mount: "/missing", writable: false },
        ],
      },
      { filesystem: { data: provider } },
    ),
  ).rejects.toThrow("Unknown filesystem provider missing");
  expect(disposed).toBe(1);
});

test("mount disposal drains every provider even when one destructor fails", async () => {
  const disposed: string[] = [];
  const provider: FilesystemProvider = {
    open(path, writable) {
      const mount = memoryFilesystem().open(path, writable);
      return {
        writable: mount.writable,
        openRoot: () => mount.openRoot(),
        async dispose() {
          await mount.dispose();
          disposed.push(path);
          if (path === "/first") throw new Error("destructor failed");
        },
      };
    },
  };
  const io = await BrowserCapabilities.open(
    {
      filesystem: [
        { provider: "data", mount: "/first", writable: false },
        { provider: "data", mount: "/second", writable: false },
      ],
    },
    { filesystem: { data: provider } },
  );
  await expect(io.dispose()).rejects.toThrow("Filesystem disposal failed");
  expect(disposed.sort()).toEqual(["/first", "/second"]);
  expect(io.mounts).toEqual([]);
  await io.dispose();
});

test("filesystem grants do not authorize network providers", async () => {
  let connects = 0;
  const io = await BrowserCapabilities.open(
    { filesystem: [{ provider: "data", mount: "/data", writable: true }] },
    {
      filesystem: { data: memoryFilesystem() },
      network: {
        socket: {
          async connect() {
            connects++;
            throw new Error("must not be called");
          },
        },
      },
    },
  );
  try {
    await expect(
      io.execute(
        { operation: "transport.connect", ip: "127.0.0.1", port: 80 },
        new AbortController().signal,
      ),
    ).rejects.toThrow();
    expect(connects).toBe(0);
  } finally {
    await io.dispose();
  }
});
