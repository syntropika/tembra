import { expect, test } from "bun:test";
import { readonlyDescriptor } from "../src/interpreter/filesystem.ts";

test("read-only authority follows asynchronous descendants and descriptor flags", async () => {
  let writes = 0;
  const descriptor: any = {
    openAt: async () => descriptor,
    getFlags: async () => ({ read: true, write: true, mutateDirectory: true }),
    write() {
      writes++;
    },
    read: () => "original",
  };
  const root = readonlyDescriptor(descriptor);
  const child = await root.openAt({}, "child", {}, {});
  const grandchild = await child.openAt({}, "grandchild", {}, {});
  expect(await grandchild.getFlags()).toEqual({ read: true, write: false, mutateDirectory: false });
  expect(grandchild.read()).toBe("original");
  for (const item of [root, child, grandchild]) {
    expect(() => item.write()).toThrow();
    expect(() => item.openAt({}, "new", { create: true }, {})).toThrow();
    expect(() => item.openAt({}, "existing", {}, { write: true })).toThrow();
  }
  expect(writes).toBe(0);
});
