/** Generated clients use the SDK's existing captured delivery and resource ownership lane. */
import type { CapturedInvocation, ResourceCapability } from "#tembra-adapters";
import type { DynamicReference } from "../authoring/types.ts";
import { bindingName } from "../interpreter/component/names.ts";
import { resourceRoles, resourceDataFunction } from "../interpreter/component/resources.ts";
import type { GraphFunction, TypeGraph } from "../interpreter/component/values.ts";
import { encode, VALUE_LIMIT } from "../interpreter/codec.ts";
import { shapes, lift } from "../interpreter/wit-shape.ts";
import { Context, context, isContext } from "./context.ts";
import { referenceOrigin } from "./references.ts";
import { checkSignal } from "./signals.ts";

/** Public ownership operations of a provider-issued WIT resource. */
export interface WitResource {
  readonly resourceType: string;
  /** Release this resource through its existing SDK owner. */
  release(): void;
}

/** Build-time checked metadata retained by generated resource clients. */
export interface WitClientSchema extends TypeGraph {
  readonly imports?: readonly { readonly binding: string; readonly contract?: string }[];
  readonly identity: string;
  readonly signature: string;
  readonly functions: Readonly<Record<string, GraphFunction>>;
}

/** An awaitable invocation of one generated, precisely typed WIT signature. */
export interface WitClient {
  call<T>(method: string, args: readonly unknown[]): Promise<T>;
}

const owners = new WeakMap<object, { owner: ResourceCapability; session: object }>();
const finalizer =
  typeof FinalizationRegistry === "undefined"
    ? undefined
    : new FinalizationRegistry<ResourceCapability>((owner) => owner.close());

/** Bind typed wrappers to an SDK reference without creating another capability protocol. */
export function witClient(reference: DynamicReference, schema: WitClientSchema): WitClient {
  const { session, record } = referenceOrigin(reference);
  const compile = shapes(schema);
  const functions = new Map(
    Object.entries(schema.functions).map(([name, fn]) => {
      const roles = resourceRoles(schema, schema.identity, { ...fn, name });
      if (roles.stream) throw new Error("Generated clients do not support stream results");
      return [name, { roles, shape: compile(resourceDataFunction(fn, roles)), fn }] as const;
    }),
  );
  /** Wrap one existing owner; its methods continue to call on its own admission claim. */
  const wrap = (owner: ResourceCapability): WitResource => {
    const value: WitResource & Record<string, unknown> = {
      resourceType: `${owner.nominal.interface}#${owner.nominal.name}`,
      release() {
        finalizer?.unregister(value);
        owner.close();
      },
    };
    owners.set(value, { owner, session });
    finalizer?.register(value, owner, value);
    for (const [name, { roles }] of functions) {
      if (roles.receiver?.name === owner.nominal.name && roles.member) {
        const method = schema.javascriptNames[roles.member] ?? bindingName(roles.member, false);
        value[method] = (...args: unknown[]) => invoke(name, args, owner);
      }
    }
    return value;
  };
  /** Keep cancellation observable while physical settlement retains the existing owners. */
  const invoke = async (
    name: string,
    supplied: readonly unknown[],
    receiver?: ResourceCapability,
  ): Promise<unknown> => {
    const selected = functions.get(name);
    if (!selected) throw new Error(`Unknown WIT function ${name}`);
    const { roles, shape } = selected;
    const args = [...supplied];
    const count = selected.fn.params.length - Number(Boolean(roles.receiver));
    if (args.length === count + 1 && args[count] === undefined) args.pop();
    const last = args.at(-1);
    const ctx = isContext(last) ? context(args.pop() as Context, session) : record.context;
    const moved: ResourceCapability[] = [];
    let accepted = false;
    const call = async (ctx: Context): Promise<unknown> => {
      let capture: CapturedInvocation | undefined;
      try {
        const arguments_ = roles.arguments
          ? (roles.arguments(args, (nominal, value) => {
              const held =
                typeof value === "object" && value !== null ? owners.get(value) : undefined;
              if (
                !held ||
                held.session !== session ||
                held.owner.nominal.interface !== nominal.interface ||
                held.owner.nominal.name !== nominal.name
              )
                throw new Error("Expected an SDK resource of the exact nominal type and host");
              const owner = held.owner.move();
              finalizer?.unregister(value as object);
              moved.push(owner);
              return moved.length - 1;
            }) as unknown[])
          : args;
        const input = encode(arguments_, VALUE_LIMIT, shape.params);
        capture = {
          signature: schema.signature,
          input,
          ...(receiver ? { receiver } : {}),
          ...(moved.length
            ? {
                arguments: {
                  owners: moved,
                  get accepted() {
                    return accepted;
                  },
                  accept() {
                    accepted = true;
                  },
                },
              }
            : {}),
        };
        const target = receiver ? receiver.reference : record;
        await session.calls.invokePhysical(target, name, arguments_, ctx, capture);
        checkSignal(ctx.signal);
        if (!capture.output)
          throw new Error("Provider does not support the matching WIT capability signature");
        const value: unknown = lift(shape.result, JSON.parse(capture.output.text));
        const capabilities = capture.capabilities ?? [];
        const consumed = new Set<number>();
        const result = roles.result
          ? roles.result(value, (nominal, index) => {
              if (typeof index !== "number" || !Number.isInteger(index) || consumed.has(index))
                throw new Error("An owned result must redeem each attachment exactly once");
              const owner = capabilities[index];
              if (
                owner?.kind !== "resource" ||
                owner.nominal.interface !== nominal.interface ||
                owner.nominal.name !== nominal.name
              )
                throw new Error("Resource has another nominal type");
              consumed.add(index);
              return wrap(owner);
            })
          : value;
        if (consumed.size !== capabilities.length)
          throw new Error("Unused result capability attachment");
        return selected.fn.result == null ? undefined : result;
      } catch (error) {
        for (const capability of capture?.capabilities ?? []) capability.close();
        throw error;
      } finally {
        if (!accepted) for (const owner of moved) owner.close();
      }
    };
    // The physical operation performs delivery checks and closes undelivered owners even if
    // cancellation wins the observation race before the provider publishes its result.
    return ctx
      ? session.calls.observe(call(ctx), ctx.signal)
      : session.calls.root(
          (ctx) => session.calls.observe(call(ctx), ctx.signal),
          false,
          undefined,
          record.address.principal,
        );
  };
  return { call: <T>(name: string, args: readonly unknown[]) => invoke(name, args) as Promise<T> };
}
