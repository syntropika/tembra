import { expect, test } from "bun:test";
import { runInNewContext } from "node:vm";
import { Code, Lifetime, Reason } from "../src/identifiers.ts";
import { Errors, type ErrorDescriptor } from "../src/errors.ts";
import { failure, TembraError } from "../src/error.ts";
import { EffectType, KernelOp } from "../src/protocol.ts";
import { createHost } from "../src/node.ts";
import type { Kernel, KernelOutcome } from "../src/kernel.ts";

test("structured errors retain their fields, default message, cause and custom orb reasons", () => {
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
  const response = (
    host.session.kernel.port.command as unknown as (
      op: string,
      input: object,
    ) => KernelOutcome<null>
  )("scope-activte", {});
  expect(response.error).toMatchObject({ code: Code.Unsupported, reason: Reason.Operation });
  expect(host.diagnostics()).toEqual(before);
  expect(() => host.session.kernel.command(KernelOp.ScopeActivate, { scope: 0 })).toThrow(
    TembraError,
  );
  await host.close();
});

test("structured failures accept foreign realms and custom reasons but reject malformed fields", () => {
  const descriptor = { code: Code.Invalid, reason: "quota", message: "Quota exceeded" };
  const foreign: unknown = runInNewContext(
    "Object.assign(new Error(message), { code, reason })",
    descriptor,
  );
  expect(foreign).not.toBeInstanceOf(Error);
  for (const input of [descriptor, foreign]) {
    expect(failure(input)).toBeInstanceOf(TembraError);
    expect(failure(input)).toMatchObject({ ...descriptor, cause: input });
  }
  for (const input of [
    null,
    undefined,
    "failure",
    { ...descriptor, code: "custom" },
    { ...descriptor, code: "toString" },
    { ...descriptor, code: null },
    { ...descriptor, reason: 1 },
    { ...descriptor, message: undefined },
  ]) {
    expect(failure(input)).toMatchObject({
      code: Code.Unavailable,
      reason: Reason.Callback,
      cause: input,
    });
  }
});

// These calls are never executed: tsc checks that closed names cannot silently widen to string.
function rejectedNames(kernel: Kernel): void {
  // @ts-expect-error Misspelled operations are not part of the closed adapter protocol.
  kernel.command("scope-activte");
  // @ts-expect-error A lifetime is not an arbitrary string.
  const lifetime: Lifetime = "forever";
  // @ts-expect-error Error categories are closed even when orb-defined reasons are extensible.
  const code: Code = "custom";
  // @ts-expect-error Effect names are closed.
  const effect: EffectType = "restart";
  void [lifetime, code, effect];
}
void rejectedNames;
