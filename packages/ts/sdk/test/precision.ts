import { KernelOp, EffectType, ActionType, OwnerKind } from "../src/protocol.ts";
import type { Action, KernelPort } from "../src/kernel.ts";

/** Shared assertions run against the real Node and browser/Worker WASM bindings. */
export function verifyIdentifierPrecision(create: () => KernelPort): number {
  const maximum = (1n << 53n) - 1n;
  const values = [
    (1n << 32n) - 1n,
    1n << 32n,
    (1n << 32n) + 1n,
    (1n << 52n) - 1n,
    1n << 52n,
    (1n << 52n) + 1n,
    maximum - 2n,
    maximum - 1n,
    maximum,
  ];
  const check = (condition: boolean, message: string) => {
    if (!condition) throw new Error(message);
  };
  const exact = (actual: unknown, expected: bigint) =>
    check(
      typeof actual === "number" && Number.isSafeInteger(actual) && BigInt(actual) === expected,
      `Identity changed: ${String(actual)} != ${expected}`,
    );
  for (const expected of values) {
    const port = create();
    const request = (op: string, fields: Record<string, unknown> = {}) =>
      JSON.parse(port.request(JSON.stringify({ op, ...fields })));
    const command = (op: string, fields: Record<string, unknown> = {}): any => {
      const result = request(op, fields);
      if (result.error) throw new Error(JSON.stringify(result.error));
      return result.ok;
    };
    try {
      const id = Number(expected);
      const parent = id - 1;
      const resource = command(KernelOp.Resource, {
        owner: { kind: OwnerKind.Host },
        name: "precision",
        version: 1,
      }).id;
      command(KernelOp.ResourceOpened, { resource, value: id });
      for (const [scope, parents, resources] of [
        [parent, [], [resource]],
        [id, [parent], []],
      ] as const)
        command(KernelOp.ScopeRegister, {
          scope,
          owner: { kind: OwnerKind.Host },
          orb: 0,
          parents,
          resources,
        });
      exact(command(KernelOp.ResourceValue, { resource }), expected);
      const scopes = command(KernelOp.ScopesForOwner, { owner: { kind: OwnerKind.Host } });
      check(scopes.length === 2, "Distinct neighboring scopes merged");
      exact(scopes[0], expected - 1n);
      exact(scopes[1], expected);

      const before = JSON.stringify(command(KernelOp.Diagnostics));
      for (const invalid of [
        "9007199254740992",
        "9007199254740993",
        "18446744073709551615",
        "-1",
        "1.5",
        "1e100",
      ]) {
        const result = JSON.parse(
          port.request(`{"op":"${KernelOp.ScopeExists}","scope":${invalid}}`),
        );
        check(
          result.error?.code === "invalid" && result.error?.reason === "metadata",
          `Unsafe identity accepted: ${invalid}`,
        );
      }
      for (const fields of [
        { scopes: [Number(maximum) + 1] },
        { scopes: [], object: Number(maximum) + 1 },
      ])
        check(
          request(KernelOp.ScopesCleanup, fields).error?.code === "invalid",
          "Unsafe array/optional identity accepted",
        );
      check(
        JSON.stringify(command(KernelOp.Diagnostics)) === before,
        "Invalid request mutated the host",
      );
      check(
        request(KernelOp.Resource, {
          owner: { kind: OwnerKind.Host },
          name: "invalid-version",
          version: 2 ** 32,
        }).error?.code === "invalid",
        "Widened identities must not widen versions",
      );
      if (command(KernelOp.Features).subscriptions) {
        check(
          request(KernelOp.Subscribe, {
            owner: { kind: OwnerKind.Host },
            source: id,
            operation: parent,
            maxPending: 2 ** 32,
          }).error?.code === "invalid",
          "Queue limit would truncate on wasm32",
        );
      }
      check(
        JSON.stringify(command(KernelOp.Diagnostics)) === before,
        "Invalid limits mutated the host",
      );
      exact(command(KernelOp.ResourceValue, { resource }), expected);

      const program = command(KernelOp.ScopesCleanup, { scopes: [id, parent] });
      command(KernelOp.ProgramRun, { program });
      let complete = false;
      let droppedValue = false;
      const droppedScopes: number[] = [];
      for (let batch = 0; batch < 16 && !complete; batch++) {
        for (const action of command(KernelOp.Drain) as Action[]) {
          if (action.type === ActionType.Execute) {
            for (const effect of action.effects!) {
              if (effect.scope !== undefined)
                check(effect.scope === id || effect.scope === parent, "Effect identity changed");
              if (effect.type === EffectType.Close)
                command(KernelOp.ResourceClosed, { resource: effect.resource });
              if (effect.type === EffectType.Drop) droppedScopes.push(effect.scope!);
            }
            command(KernelOp.ProgramCompleteStep, {
              program,
              results: action.effects!.map(() => ({ value: null })),
            });
          } else if (action.type === ActionType.DropValue) {
            exact(action.id, expected);
            droppedValue = true;
          } else if (action.type === ActionType.ProgramComplete) {
            check(!action.error, "Cleanup failed");
            complete = true;
          }
        }
      }
      check(complete && droppedValue, "Cleanup did not preserve its value identity");
      check(droppedScopes.length === 2, "Cleanup lost a scope");
      exact(droppedScopes[0], expected);
      exact(droppedScopes[1], expected - 1n);
      command(KernelOp.Close);
      command(KernelOp.FinishClose);
    } finally {
      port.free?.();
    }
  }
  return values.length;
}
