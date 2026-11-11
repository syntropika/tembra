/** Provider-owned WIT resources of the bounded compiled capability profile. */
import type { ReferenceRecord } from "../../runtime/records.ts";
import type { Session } from "../../runtime/session.ts";
import type { ResourceCapability } from "#tembra-adapters";
import type { GraphFunction, TypeGraph } from "./values.ts";
import { bindingName } from "./host.ts";

export interface Nominal {
  readonly interface: string;
  readonly name: string;
}
export interface ResourceRoles {
  readonly receiver?: Nominal | undefined;
  readonly owned?: readonly Nominal[] | undefined;
  /** Map only own positions in the host-domain result; other values remain ordinary data. */
  readonly result?:
    | ((value: unknown, own: (nominal: Nominal, value: unknown) => unknown) => unknown)
    | undefined;
  readonly stream?: boolean | undefined;
  readonly resource?: Nominal | undefined;
  readonly member?: string | undefined;
  readonly construct?: boolean | undefined;
}
const same = (a: Nominal, b: Nominal) => a.interface === b.interface && a.name === b.name;

/** An owner never grants authority: every method uses its exact duplicated kernel claim. */
export class ResourceOwner implements ResourceCapability {
  readonly kind = "resource";
  #anchor: { session: Session; reference: ReferenceRecord } | undefined;
  #borrows = 0;
  #closing = false;
  #closed = false;
  #settled = false;
  readonly #closedCallbacks = new Set<() => void>();
  constructor(
    readonly nominal: Nominal,
    readonly issuer: object,
    readonly environment: number,
    readonly value: object,
    readonly destroy: () => void | Promise<void>,
  ) {}

  anchor(session: Session, reference: ReferenceRecord): void {
    if (this.#closing || this.#anchor) throw new Error("Resource was already delivered or closed");
    this.#anchor = { session, reference: session.handles.duplicate(reference) };
  }
  get reference(): ReferenceRecord {
    if (this.#closing || !this.#anchor) throw new Error("Resource is not live");
    return this.#anchor.reference;
  }
  /** Physical calls pin the resource even when its guest owner has already dropped it. */
  lend(): () => void {
    if (this.#closing) throw new Error("Resource is not live");
    this.#borrows++;
    let ended = false;
    return () => {
      if (ended) return;
      ended = true;
      this.#borrows--;
      this.#finish();
    };
  }
  redeem(issuer: object, environment: number, expected: Nominal): object {
    if (this.#closed || this.issuer !== issuer || this.environment !== environment)
      throw new Error("Resource belongs to another instance or a retired revision");
    if (!same(this.nominal, expected)) throw new Error("Resource has another nominal type");
    return this.value;
  }
  whenClosed(operation: () => void): void {
    if (this.#settled) operation();
    else this.#closedCallbacks.add(operation);
  }
  close(): void {
    this.#closing = true;
    this.#finish();
  }
  #finish(): void {
    if (!this.#closing || this.#closed || this.#borrows) return;
    this.#closed = true;
    const finish = () => {
      if (this.#anchor) this.#anchor.session.handles.release(this.#anchor.reference.id);
      this.#anchor = undefined;
      this.#settled = true;
      for (const operation of this.#closedCallbacks) operation();
      this.#closedCallbacks.clear();
    };
    try {
      const pending = this.destroy();
      if (pending) {
        void pending.then(finish, finish);
        return;
      }
    } catch (error) {
      finish();
      throw error;
    }
    finish();
  }
}

/** Read roles already admitted by the Rust resource profile; handles remain outside data. */
export function resourceRoles(graph: TypeGraph, binding: string, fn: GraphFunction): ResourceRoles {
  const definitions = new Map(graph.types.map(({ id, definition }) => [id, definition]));
  const resolve = (type: unknown): unknown => {
    for (let depth = 0; typeof type === "number" && depth < 64; depth++) {
      const kind = definitions.get(type)?.kind;
      if (typeof kind !== "object" || kind.type === undefined) break;
      type = kind.type;
    }
    return type;
  };
  const nominal = (id: unknown): Nominal => {
    const definition = typeof id === "number" ? definitions.get(resolve(id) as number) : undefined;
    if (definition?.kind !== "resource" || !definition.owner || !definition.name)
      throw new Error("Resource has no portable nominal identity");
    if (definition.owner !== binding) throw new Error("Resource requires its defining interface");
    return { interface: definition.owner, name: definition.name };
  };
  const kind = fn.kind;
  let receiver: Nominal | undefined;
  let resource: Nominal | undefined;
  let constructor = false;
  let member: string | undefined;
  if (typeof kind === "object" && kind !== null) {
    const [form, id] = Object.entries(kind)[0] ?? [];
    resource = nominal(id);
    constructor = form === "constructor";
    if (form === "method" || form === "async-method") receiver = resource;
    if (!constructor) member = fn.name?.split(".").at(-1);
  }
  const owned = new Map<string, Nominal>();
  type MapOwn = (value: unknown, own: (nominal: Nominal, value: unknown) => unknown) => unknown;
  const mapper = (type: unknown): MapOwn | undefined => {
    const kind = definitions.get(resolve(type) as number)?.kind;
    if (typeof kind !== "object") return undefined;
    if (kind.handle?.own !== undefined) {
      const identity = nominal(kind.handle.own);
      owned.set(identity.name, identity);
      return (value, own) => own(identity, value);
    }
    if (kind.list !== undefined) {
      const item = mapper(kind.list);
      return item
        ? (value, own) => (value as unknown[]).map((value) => item(value, own))
        : undefined;
    }
    if (kind.option !== undefined) {
      const inner = mapper(kind.option);
      return inner ? (value, own) => (value == null ? value : inner(value, own)) : undefined;
    }
    if (kind.record) {
      const fields = kind.record.fields.map(
        ({ name, type }) => [graph.javascriptNames[name]!, mapper(type)] as const,
      );
      if (!fields.some(([, map]) => map)) return undefined;
      return (value, own) => {
        const output = { ...(value as Record<string, unknown>) };
        for (const [name, map] of fields) if (map) output[name] = map(output[name], own);
        return output;
      };
    }
    if (kind.tuple) {
      const fields = kind.tuple.types.map(mapper);
      return fields.some(Boolean)
        ? (value, own) =>
            (value as unknown[]).map((value, index) => {
              const map = fields[index];
              return map ? map(value, own) : value;
            })
        : undefined;
    }
    if (kind.variant || kind.result) {
      const cases = kind.variant?.cases ?? [
        { name: "ok", type: kind.result!.ok },
        { name: "err", type: kind.result!.err },
      ];
      const maps = new Map(cases.map(({ name, type }) => [name, mapper(type)]));
      if (![...maps.values()].some(Boolean)) return undefined;
      return (value, own) => {
        const tagged = value as { tag: string; val?: unknown };
        const map = maps.get(tagged.tag);
        return map ? { tag: tagged.tag, val: map(tagged.val, own) } : value;
      };
    }
    return undefined;
  };
  const resultKind = definitions.get(resolve(fn.result) as number)?.kind;
  const stream = typeof resultKind === "object" && resolve(resultKind.stream) === "u8";
  const result = mapper(fn.result);
  return {
    receiver,
    owned: owned.size ? [...owned.values()] : undefined,
    result,
    stream,
    resource,
    member,
    construct: constructor,
  };
}

/** The signature of the checked data lane: self and whole capabilities travel separately. */
export function resourceDataFunction(fn: GraphFunction, roles: ResourceRoles): GraphFunction {
  return {
    ...fn,
    params: roles.receiver ? fn.params.slice(1) : fn.params,
    result: roles.stream ? undefined : fn.result,
  };
}

/** Resolve the JCO resource class/member rather than its flattened contract spelling. */
export function resourceExport(
  exports: Record<string, unknown>,
  spelling: string,
  roles: ResourceRoles,
  receiver: object | undefined,
  args: readonly unknown[],
): unknown {
  if (!roles.resource) {
    const fn = exports[spelling];
    if (typeof fn !== "function") throw new Error(`Missing component export ${spelling}`);
    return fn(...args);
  }
  const Resource = exports[bindingName(roles.resource.name, true)] as
    | (new (...args: unknown[]) => unknown)
    | undefined;
  if (typeof Resource !== "function") throw new Error("Missing component resource class");
  if (roles.construct) return new Resource(...args);
  const target = roles.receiver ? receiver : Resource;
  const fn = (target as Record<string, unknown> | undefined)?.[bindingName(roles.member!, false)];
  if (typeof fn !== "function") throw new Error("Missing component resource method");
  return fn.apply(target, args);
}
