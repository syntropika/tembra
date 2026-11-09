import { expect, test } from "bun:test";
import { memoryFilesystem } from "../src/interpreter/filesystem.ts";
import { BrowserCapabilities } from "../src/interpreter/io.ts";

test("mount permissions and disposal are independent over shared data", async () => {
  const provider = memoryFilesystem({ "nested/input.txt": "original" });
  const writer = provider.open("/data", true);
  const reader = provider.open("/data", false);
  await expect(reader.writeText("nested/input.txt", "forbidden")).rejects.toThrow("read-only");
  await writer.writeText("nested/input.txt", "updated");
  expect(await reader.readText("nested/input.txt")).toBe("updated");
  await writer.dispose();
  await expect(writer.readText("nested/input.txt")).rejects.toThrow("closed");
  expect(await reader.readText("nested/input.txt")).toBe("updated");
  await reader.dispose();
});

test("host I/O rejects traversal, mount-prefix confusion and ungranted transport", async () => {
  const io = await BrowserCapabilities.open(
    { filesystem: [{ provider: "data", mount: "/data", writable: true }] },
    { data: memoryFilesystem({ "input.txt": "value" }) },
  );
  for (const path of [
    "/data/../input.txt",
    "/data//input.txt",
    "/database/input.txt",
    "/input.txt",
    "/data/./input.txt",
    "/data/a\\b",
    "/data/a\0b",
  ])
    await expect(io.execute({ operation: "filesystem.read-text", path })).rejects.toThrow();
  await expect(
    io.execute({ operation: "transport.exchange", ip: "127.0.0.1", port: 80, request: "GET /" }),
  ).rejects.toThrow("no raw TCP");
  expect(await io.execute({ operation: "filesystem.read-text", path: "/data/input.txt" })).toBe(
    "value",
  );
  await io.dispose();
  await expect(
    io.execute({ operation: "filesystem.read-text", path: "/data/input.txt" }),
  ).rejects.toThrow();
});

test("the most specific mount cannot inherit a broader write grant", async () => {
  const provider = memoryFilesystem({ "input.txt": "protected", "nested/input.txt": "broader" });
  const io = await BrowserCapabilities.open(
    {
      filesystem: [
        { provider: "data", mount: "/data", writable: true },
        { provider: "data", mount: "/data/nested", writable: false },
      ],
    },
    { data: provider },
  );
  await expect(
    io.execute({
      operation: "filesystem.write-text",
      path: "/data/nested/input.txt",
      value: "forbidden",
    }),
  ).rejects.toThrow("read-only");
  expect(
    await io.execute({ operation: "filesystem.read-text", path: "/data/nested/input.txt" }),
  ).toBe("protected");
  await io.dispose();
});

test("the host enforces read-only grants independently of a custom provider", async () => {
  let writes = 0;
  const io = await BrowserCapabilities.open(
    { filesystem: [{ provider: "custom", mount: "/data", writable: false }] },
    {
      custom: {
        open: () => ({
          readText: async () => "value",
          writeText: async () => {
            writes++;
          },
          dispose() {},
        }),
      },
    },
  );
  await expect(
    io.execute({ operation: "filesystem.write-text", path: "/data/input", value: "forbidden" }),
  ).rejects.toThrow("read-only");
  expect(writes).toBe(0);
  expect(await io.execute({ operation: "filesystem.read-text", path: "/data/input" })).toBe(
    "value",
  );
  await io.dispose();
});

test("custom providers cannot bypass text bounds or return invalid text", async () => {
  let reads: unknown = "x".repeat((1 << 20) + 1);
  let writes = 0;
  const io = await BrowserCapabilities.open(
    { filesystem: [{ provider: "custom", mount: "/data", writable: true }] },
    {
      custom: {
        open: () => ({
          readText: async () => reads as string,
          writeText: async () => {
            writes++;
          },
          dispose() {},
        }),
      },
    },
  );
  await expect(
    io.execute({ operation: "filesystem.read-text", path: "/data/input" }),
  ).rejects.toThrow("limit");
  for (const value of ["x".repeat((1 << 20) + 1), "界".repeat(400000)])
    await expect(
      io.execute({ operation: "filesystem.write-text", path: "/data/input", value }),
    ).rejects.toThrow("limit");
  expect(writes).toBe(0);
  reads = 42;
  await expect(
    io.execute({ operation: "filesystem.read-text", path: "/data/input" }),
  ).rejects.toThrow("string");
  await io.dispose();
});
