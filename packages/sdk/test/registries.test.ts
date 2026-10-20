import { expect, test } from "bun:test";

import { canonicalJson, defineRegistry } from "../src/index.js";

test("registry definitions stay portable, immutable and independent of any Host", () => {
  const commands = defineRegistry<{ title: string; description?: string }>("app.commands", {
    conflict: "error",
  });
  expect(commands).toEqual({ name: "app.commands", conflict: "error" });
  expect(Object.isFrozen(commands)).toBe(true);
  expect(Reflect.ownKeys(commands)).toEqual(["name", "conflict"]);
  expect(canonicalJson(commands)).toBe('{"conflict":"error","name":"app.commands"}');
  expect(defineRegistry("default").conflict).toBe("error");
  expect(defineRegistry("last", { conflict: "last" }).conflict).toBe("last");
});

test("registry definitions refuse invalid names and policies", () => {
  expect(() => defineRegistry("")).toThrow("nonempty");
  expect(() => defineRegistry("\uD800")).toThrow("surrogate");
  // @ts-expect-error: JavaScript callers still get a checked policy boundary.
  expect(() => defineRegistry("invalid", { conflict: "replace" })).toThrow("policy");
});
