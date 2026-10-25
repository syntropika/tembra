import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { Code, Lifetime, Reason, ReloadStatus, Status } from "../src/identifiers.ts";
import { Errors, type ErrorDescriptor } from "../src/errors.ts";
import { failure, TembraError } from "../src/error.ts";
import { ActionType, EffectType, KernelOp, NativeOp, StepType } from "../src/protocol.ts";
import { createHost } from "../src/node.ts";
import type { Kernel } from "../src/kernel.ts";

test("JavaScript and Rust use the same closed wire vocabulary", () => {
  const source = readFileSync(
    new URL("../../../crates/bridge/src/protocol.rs", import.meta.url),
    "utf8",
  );
  for (const [name, values] of Object.entries({
    KernelOp,
    ActionType,
    EffectType,
    StepType,
    NativeOp,
  })) {
    const block = source.match(new RegExp(`${name} \\{([^}]+)\\}`))?.[1];
    expect(block).toBeDefined();
    const rust = [...block!.matchAll(/(\w+) => "([^"]+)"/g)].map(([, name, value]) => [
      name,
      value,
    ]);
    expect(Object.fromEntries(rust)).toEqual(values);
  }
  expect(Lifetime.Host).toBe("host");
  expect(Status.Live).toBe("live");
  expect(ReloadStatus.Prepared).toBe("prepared");
});

test("structured errors retain their fields, default message, cause and custom facet reasons", () => {
  const descriptor = Errors.Invalid.StateValidation;
  const error = new TembraError(descriptor);
  expect(error).toBeInstanceOf(Error);
  expect(error).toMatchObject(descriptor);
  expect(Object.isFrozen(descriptor)).toBe(true);
  expect(Object.isFrozen(Errors.Invalid)).toBe(true);
  const cause = new Error("storage disconnected");
  expect(failure(cause)).toMatchObject({
    code: Code.Unavailable,
    reason: Reason.Callback,
    message: cause.message,
    cause,
  });
  expect(failure(error)).toBe(error);
  const custom: ErrorDescriptor = {
    code: Code.Invalid,
    reason: "quota",
    message: "Quota exceeded",
  };
  expect(new TembraError(custom)).toMatchObject(custom);
});

test("named operations retain native wire errors and refuse unknown operations without mutation", async () => {
  const host = createHost();
  const before = host.diagnostics();
  const response = JSON.parse(
    host.session.kernel.port.request(JSON.stringify({ op: "scope-activte" })),
  );
  expect(response.error).toMatchObject({ code: Code.Unsupported, reason: Reason.Operation });
  expect(host.diagnostics()).toEqual(before);
  expect(() => host.session.kernel.command(KernelOp.ScopeActivate, { scope: 0 })).toThrow(
    TembraError,
  );
  await host.close();
});

// These calls are never executed: tsc checks that closed names cannot silently widen to string.
function rejectedNames(kernel: Kernel): void {
  // @ts-expect-error Misspelled operations are not part of the closed adapter protocol.
  kernel.command("scope-activte");
  // @ts-expect-error A lifetime is not an arbitrary string.
  const lifetime: Lifetime = "forever";
  // @ts-expect-error Error categories are closed even when facet-defined reasons are extensible.
  const code: Code = "custom";
  // @ts-expect-error Effect names are closed.
  const effect: EffectType = "restart";
  void [lifetime, code, effect];
}
void rejectedNames;
