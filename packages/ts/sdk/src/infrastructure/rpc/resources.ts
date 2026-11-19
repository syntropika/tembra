/** RPC carries existing SDK resource owners through the same connection's references. */
import { Code } from "../../bridge/identifiers.ts";
import { TembraError } from "../../error.ts";
import { Errors } from "../../errors.ts";
import { encode } from "../../interpreter/codec.ts";
import { ResourceOwner } from "../../interpreter/component/resources.ts";
import type { CapturedInvocation } from "../../runtime/adapters.ts";
import type { WitClientBackend, WitClientSchema } from "../../runtime/wit-client.ts";

/** A descriptor is useful only on its original connection and never replaces host admission. */
export interface RpcResource {
  readonly reference: number;
  readonly token: string;
  readonly interface: string;
  readonly name: string;
  readonly revision: string;
}

/** Validate server descriptors before giving the existing SDK owner authority to release them. */
export function resource(value: unknown): RpcResource {
  if (typeof value !== "object" || value === null) throw new TembraError(Errors.Invalid.RpcFrame);
  const item = value as Record<string, unknown>;
  if (
    !Number.isSafeInteger(item.reference) ||
    (item.reference as number) <= 0 ||
    typeof item.token !== "string" ||
    !/^[a-f0-9]{64}$/.test(item.token) ||
    typeof item.interface !== "string" ||
    typeof item.name !== "string" ||
    typeof item.revision !== "string" ||
    !/^[1-9][0-9]*$/.test(item.revision)
  )
    throw new TembraError(Errors.Invalid.RpcFrame);
  return item as unknown as RpcResource;
}

/** The RemoteHost supplies ordinary request, release and connection operations. */
interface RemoteDelivery {
  readonly identity: object;
  readonly reference: number;
  readonly resources: unknown;
  readonly signature: unknown;
  /** Check service release only for calls that have no independent resource receiver. */
  check(): void;
  /** Correlate captured delivery with the existing RPC cancellation path. */
  request(
    payload: unknown,
    capture: CapturedInvocation,
    received: (value: unknown) => void,
    signal?: AbortSignal,
  ): Promise<unknown>;
  /** Release through the existing connection claim table. */
  release(resource: RpcResource): void;
}

/** Bind one generated client without a browser component or a second ownership implementation. */
export function resourceBackend(remote: RemoteDelivery, schema: WitClientSchema): WitClientBackend {
  if (remote.resources !== 1 || remote.signature !== schema.signature)
    throw new TembraError({
      code: Code.Unsupported,
      reason: "capabilities",
      message: "Matching WIT resource delivery is required",
    });
  return {
    identity: remote.identity,
    run(ctx, operation) {
      if (ctx) ctx.check();
      return operation(ctx);
    },
    async invoke(method, _args, capture, ctx) {
      if (!capture.receiver) remote.check();
      const receiver = capture.receiver
        ? resource(capture.receiver.redeem(remote.identity, 0, capture.receiver.nominal))
        : undefined;
      const owned = (capture.arguments?.owners ?? []).map((owner) =>
        resource(owner.redeem(remote.identity, 0, owner.nominal)),
      );
      // Ownership stays revoked even if the request's delivery later becomes uncertain.
      for (const owner of capture.arguments?.owners ?? [])
        owner.take(remote.identity, owner.nominal);
      capture.arguments?.accept();
      try {
        await remote.request(
          {
            reference: receiver?.reference ?? remote.reference,
            method,
            input: capture.input.text,
            capabilities: { signature: schema.signature, receiver: receiver ?? null, owned },
          },
          capture,
          (value) => {
            if (
              typeof value !== "object" ||
              value === null ||
              !Array.isArray((value as Record<string, unknown>).owned)
            )
              throw new TembraError(Errors.Invalid.RpcFrame);
            const result = value as { output?: unknown; owned: unknown[] };
            if (typeof result.output !== "string") throw new TembraError(Errors.Invalid.RpcFrame);
            const descriptors = result.owned.map(resource);
            if (new Set(descriptors.map((item) => item.reference)).size !== descriptors.length)
              throw new TembraError(Errors.Invalid.RpcFrame);
            capture.capabilities = descriptors.map(
              (item) =>
                new ResourceOwner(
                  { interface: item.interface, name: item.name },
                  remote.identity,
                  0,
                  item,
                  () => remote.release(item),
                ),
            );
            capture.output = encode(JSON.parse(result.output) as unknown);
          },
          ctx?.signal,
        );
      } catch (error) {
        for (const item of owned) remote.release(item);
        throw error;
      }
    },
  };
}
