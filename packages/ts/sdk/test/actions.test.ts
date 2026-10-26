import { expect, test } from "bun:test";
import { createHost } from "../src/node.ts";
import { Code, Reason } from "../src/identifiers.ts";
import { Errors } from "../src/errors.ts";
import { TembraError } from "../src/error.ts";
import { jsonCodec, utf8 } from "../src/codec.ts";
import { KernelOp } from "../src/protocol.ts";
import { deferred } from "../src/runtime/records.ts";
import type { Reload } from "../src/kernel.ts";

for (const failure of ["decode", "drop", "unprintable"] as const)
  for (const rejects of [false, true])
    test(`${failure} action failure preserves a ${rejects ? "rejected" : "successful"} call and subsequent disposal`, async () => {
      const started = deferred<void>();
      const finish = deferred<void>();
      const values = new Map<number, { count: number }>();
      const disposed: number[] = [];
      const callbackError =
        failure === "unprintable" ? Object.create(null) : new Error("Native drop failed");
      const methodError = new TembraError(Errors.Forbidden.Method, "Native method rejected");
      let orbId = 0;
      let failingValue = 0;
      let reported = 0;
      const host = createHost({
        onError() {
          reported++;
          throw new Error("Error observer failed");
        },
        native: {
          hasOrb: (id) => id === orbId,
          activate: async (object) => {
            const value = host.session.kernel.command<number>(KernelOp.Allocate);
            values.set(value, { count: 0 });
            host.session.kernel.command(KernelOp.StateInit, { object, value, version: 1 });
          },
          invoke: async (invocation) => {
            started.resolve();
            await finish.promise;
            const previous = host.session.snapshot(invocation.object);
            failingValue = previous.value;
            const value = host.session.kernel.command<number>(KernelOp.Allocate);
            values.set(value, { count: failure === "decode" ? 2 ** 60 : 1 });
            host.session.kernel.command(KernelOp.StateAdopt, {
              call: invocation.id,
              object: invocation.object,
              revision: previous.revision,
              value,
            });
            if (rejects) throw methodError;
            return "native-result";
          },
          value: (id) => jsonCodec.deserialize(utf8(JSON.stringify(values.get(id)))),
          dropValue: (id) => {
            values.delete(id);
            if (failure !== "decode" && id === failingValue) throw callbackError;
          },
          dispose: async (object) => {
            disposed.push(object);
          },
          effect: async () => {},
        },
      });
      const prepared = host.session.kernel.command<Reload>(KernelOp.Prepare, {
        orbs: [
          { name: "native", apiVersion: 1, services: [{ name: "native", methods: ["update"] }] },
        ],
      });
      orbId = prepared.orbs[0]!.id;
      host.session.kernel.command(KernelOp.Commit, { ticket: prepared.id });
      const reference = await host.get("native", "main");
      const object = host.session.handles.reference(reference).object;
      const seen: number[] = [];
      host.session.objects
        .get(object)!
        .binding!.subscribe((value) => seen.push((value as { count: number }).count));
      const calling = reference.update();
      calling.catch(() => {});
      try {
        await started.promise;
        reference.release();
        finish.resolve();
        if (rejects) await expect(calling).rejects.toBe(methodError);
        else expect(await calling).toBe("native-result");
        await Promise.allSettled([...host.session.jobs]);
        expect(disposed).toEqual([object]);
        expect(seen).toEqual(failure === "decode" ? [0] : [0, 1]);
        const errors = host.errors();
        expect(errors).toMatchObject([
          {
            code: failure === "decode" ? Code.Invalid : Code.Unavailable,
            reason: failure === "decode" ? Reason.JsonNumber : Reason.Callback,
          },
        ]);
        if (failure !== "decode") expect(errors[0]?.cause).toBe(callbackError);
        expect(reported).toBe(1);
        const next = await host.get("native", "main");
        expect(host.session.handles.reference(next).object).not.toBe(object);
        expect(next.state.value).toEqual({ count: 0 });
        next.release();
      } finally {
        finish.resolve();
        await calling.catch(() => {});
        await host.close();
      }
      expect(values.size).toBe(0);
      expect(host.diagnostics()).toMatchObject({ objects: 0, calls: 0, pendingJobs: 0, values: 0 });
    });
