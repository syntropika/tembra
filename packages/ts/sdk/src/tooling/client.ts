/**
 * Typed TypeScript clients of a contract's exported interfaces, for host applications.
 *
 * A host acquires a WIT service by name (`host.get("<orb>::<identity>", key)`) and receives an
 * untyped `DynamicReference` whose method keys are the kebab-case WIT names. `client()` reads the
 * contract's describe graph through `NodeContract` and writes, for each exported interface, a
 * module the application commits: the interface's signatures in the host's value form (camelCase
 * record fields, `{ tag, val }` variants and results, `T | null` options, plain arrays, `bigint`
 * for 64-bit integers) and `get(scope, { orb, key })`, which acquires the reference through a
 * `Host` or a `Context` and wraps it with camelCase methods calling the kebab-case keys. The
 * wrapper only leaves out a context given as `undefined`: every value still crosses the same
 * checked boundary. Each record, variant, enum and flags type the interface uses is exported under
 * its WIT name, in its JavaScript spelling (`Point` for `point`), which the contract read from WIT
 * answers to its `type-names` query: the describe graph keeps no type names.
 */
import { bindingName } from "../interpreter/component/names.ts";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import { NodeContract } from "@syntropika/tembra-engine-node";
import type { ReferenceBase } from "../authoring/types.ts";
import { HTTP_HANDLER } from "../interpreter/http-ingress.ts";
import { query } from "../interpreter/types.ts";
import { resourceNames as resourceNaming } from "./client-resources.ts";

export interface ClientOptions {
  /** The contract's entry `.wit` file; its directory and `deps/` resolve as for `build()`. */
  wit: string;
  /** The world whose exported interfaces get clients; the package's only world when omitted. */
  world?: string;
  /** The directory of the generated modules, one `<namespace>-<package>-<interface>.ts` each. */
  out: string;
}

type WitType = string | number | null | undefined;
interface WitKind {
  type?: WitType;
  list?: WitType;
  option?: WitType;
  record?: { fields: { name: string; type: WitType }[] };
  tuple?: { types: WitType[] };
  variant?: { cases: { name: string; type?: WitType }[] };
  enum?: { cases: { name: string }[] };
  flags?: { flags: { name: string }[] };
  result?: { ok?: WitType; err?: WitType };
  handle?: { own?: number; borrow?: number };
  [other: string]: unknown;
}
interface DescribedFunction {
  readonly name: string;
  readonly kind?: string | Readonly<Record<string, number>>;
  readonly params: readonly { readonly name: string; readonly type: WitType }[];
  readonly result?: WitType;
}
/** The parts of the contract's `describe` answer this module reads. */
interface Described {
  readonly imports?: readonly { readonly binding: string; readonly contract?: string | null }[];
  readonly exports: readonly {
    readonly contract?: string | null;
    readonly type_only?: boolean;
    readonly resources?: Readonly<Record<string, number>>;
    readonly functions?: Readonly<Record<string, DescribedFunction>>;
  }[];
  readonly types: readonly {
    readonly id: number;
    readonly definition: {
      readonly kind: WitKind | string;
      readonly owner?: string;
      readonly name?: string;
    };
  }[];
  readonly javascriptNames: Readonly<Record<string, string>>;
}
/**
 * The contract's `type-names` answer: a WIT name for some ids of the describe graph, an alias's
 * own where an interface takes a type with `use`, the interface where it is written, and its
 * spelling alone and after the interface that declares the definition (`Point`, `APoint`).
 */
type TypeNames = readonly {
  readonly id: number;
  readonly name: string;
  readonly interface?: string | null;
  readonly javascript: string;
  readonly qualified: string;
}[];

const PRIMITIVES: Readonly<Record<string, string>> = {
  bool: "boolean",
  u8: "number",
  u16: "number",
  u32: "number",
  s8: "number",
  s16: "number",
  s32: "number",
  f32: "number",
  f64: "number",
  u64: "bigint",
  s64: "bigint",
  char: "string",
  string: "string",
};

/** Words that cannot name a parameter in a strict-mode module, and the trailing `context`. */
const RESERVED = new Set([
  "arguments",
  "await",
  "break",
  "case",
  "catch",
  "class",
  "const",
  "continue",
  "debugger",
  "default",
  "delete",
  "do",
  "else",
  "enum",
  "eval",
  "export",
  "extends",
  "false",
  "finally",
  "for",
  "function",
  "if",
  "implements",
  "import",
  "in",
  "instanceof",
  "interface",
  "let",
  "new",
  "null",
  "package",
  "private",
  "protected",
  "public",
  "return",
  "static",
  "super",
  "switch",
  "this",
  "throw",
  "true",
  "try",
  "typeof",
  "var",
  "void",
  "while",
  "with",
  "yield",
  "context",
]);

/**
 * Members every client has, which no service function may share: a reference serves its own
 * members before the service's methods and answers `undefined` for `then`, and `then` would make
 * the client a thenable.
 */
const MEMBERS: Readonly<Record<keyof ReferenceBase | "then" | "reference", true>> = {
  ready: true,
  status: true,
  state: true,
  release: true,
  opaque: true,
  then: true,
  reference: true,
};

/** Types the module names itself, imported or global; an interface's type takes a `_` instead. */
const OWN = new Set([
  "AcquisitionOptions",
  "Context",
  "DynamicReference",
  "Promise",
  "ReferenceBase",
  "WitResource",
  "WitClientSchema",
]);

/** Write one typed client module per exported interface; returns every module's path. */
export async function client(options: ClientOptions): Promise<readonly string[]> {
  const wit = path.resolve(options.wit);
  if (!wit.endsWith(".wit")) throw new Error("Client generation requires a .wit entry file");
  const contract = new NodeContract(path.dirname(wit), options.world);
  const graph = query<Described>(contract, { kind: "describe" });
  const names = query<TypeNames>(contract, { kind: "type-names" });
  const hasResources = graph.exports.some((entry) => Object.keys(entry.resources ?? {}).length);
  const signatures = hasResources
    ? query<Readonly<Record<string, string>>>(contract, { kind: "client-signatures" })
    : {};
  const source = `${path.basename(wit)}${options.world ? `, world ${options.world}` : ""}`;
  const out = path.resolve(options.out);
  const modules = new Map<string, string>();
  for (const entry of graph.exports) {
    const identity = entry.contract;
    if (!identity || !entry.functions || entry.type_only || identity === HTTP_HANDLER) continue;
    const file = path.join(out, `${identity.replace(/@[^@]*$/, "").replace(/[:/]/g, "-")}.ts`);
    if (modules.has(file))
      throw new Error(`${identity} and another exported interface share the module ${file}`);
    modules.set(file, clientModule(graph, names, identity, entry, source, signatures[identity]));
  }
  if (modules.size === 0) throw new Error(`${source} exports no interface a host can call`);
  await mkdir(out, { recursive: true });
  for (const [file, contents] of modules) {
    const previous = await readFile(file, "utf8").catch((error: NodeJS.ErrnoException) => {
      if (error.code === "ENOENT") return undefined;
      throw error;
    });
    if (previous === contents) continue;
    const temporary = `${file}.${process.pid}.tmp`;
    await writeFile(temporary, contents);
    await rename(temporary, file);
  }
  return [...modules.keys()].sort();
}

/** The module of one exported interface. */
function clientModule(
  graph: Described,
  names: TypeNames,
  identity: string,
  entry: Described["exports"][number],
  source: string,
  signature?: string,
): string {
  const interfaceName = /\/([^@/]+)(?:@[^@]*)?$/.exec(identity)?.[1];
  if (!interfaceName) throw new Error(`${identity} is not a versioned interface identity`);
  const spelled = graph.javascriptNames[interfaceName] ?? bindingName(interfaceName, false);
  const pascal = spelled.charAt(0).toUpperCase() + spelled.slice(1);
  const service = OWN.has(pascal) ? `${pascal}_` : pascal;
  const functions = Object.values(entry.functions ?? {});
  const occupied = new Set([...OWN, service]);
  const resourceTypes = resourceNaming(graph, entry.resources ?? {}, identity, occupied);
  const resourceNames = resourceTypes.names;
  const resources = resourceNames.size > 0;
  const { render, declarations } = renderer(
    graph,
    names,
    identity,
    occupied,
    functions.flatMap((fn) => [
      ...fn.params.filter((p) => p.name !== "self").map((p) => p.type),
      fn.result,
    ]),
    resourceNames,
  );
  const signatures: string[] = [];
  const members: string[] = [];
  const resourceSignatures = new Map([...resourceNames].map(([id]) => [id, [] as string[]]));
  const used = new Map<string, Set<string>>();
  for (const fn of functions.sort((a, b) => a.name.localeCompare(b.name))) {
    const kind = typeof fn.kind === "object" ? Object.entries(fn.kind)[0] : undefined;
    const receiver = kind?.[0] === "method" || kind?.[0] === "async-method";
    const resource = kind ? resourceNames.get(resourceTypes.canonical(kind[1])) : undefined;
    const member = fn.name.split(".").at(-1)!;
    const method =
      kind?.[0] === "constructor"
        ? `new${resource}`
        : kind
          ? receiver
            ? bindingName(member, false)
            : `${resource!.charAt(0).toLowerCase()}${resource!.slice(1)}${bindingName(member, false).charAt(0).toUpperCase()}${bindingName(member, false).slice(1)}`
          : (graph.javascriptNames[fn.name] ?? bindingName(fn.name, false));
    if (kind && !resource)
      throw new Error(`${identity}: resource function has no defining resource`);
    if (
      Object.hasOwn(MEMBERS, fn.name) ||
      Object.hasOwn(MEMBERS, method) ||
      method === "resourceType"
    )
      throw new Error(
        `${identity}: ${fn.name} collides with a member every client has (${Object.keys(MEMBERS).join(", ")})`,
      );
    const scope = receiver ? resource! : service;
    const methodNames = used.get(scope) ?? new Set<string>();
    if (methodNames.has(method))
      throw new Error(`${identity}: generated method collision: ${scope}.${method}`);
    methodNames.add(method);
    used.set(scope, methodNames);
    const taken = new Set<string>();
    const params = receiver ? fn.params.slice(1) : fn.params;
    const parameters = params.map((parameter) => {
      let name = bindingName(parameter.name, false);
      while (RESERVED.has(name) || taken.has(name)) name += "_";
      taken.add(name);
      return `${name}: ${render(parameter.type)}`;
    });
    parameters.push("context?: Context");
    const result = fn.result === undefined || fn.result === null ? "void" : render(fn.result);
    const key = method === "new" ? JSON.stringify(method) : method;
    const declaration = `  ${key}(${parameters.join(", ")}): Promise<${result}>;`;
    if (receiver) resourceSignatures.get(resourceTypes.canonical(kind![1]))!.push(declaration);
    else {
      signatures.push(declaration);
      members.push(
        resources
          ? `    ${method}: (...args) => calls.call<${result}>(${JSON.stringify(fn.name)}, sent(args, ${params.length})),`
          : `    ${method}: (...args) => reference[${JSON.stringify(fn.name)}](...sent(args, ${params.length})),`,
      );
    }
  }
  const wrappers = [...resourceNames].flatMap(([id, name]) => {
    return [
      `/** An owned WIT resource. Release it when its work ends. */`,
      `export interface ${name} extends WitResource {`,
      `  readonly resourceType: ${JSON.stringify(resourceTypes.nominals.get(id))};`,
      ...resourceSignatures.get(id)!,
      `}`,
      ``,
    ];
  });
  const schema = resources
    ? [
        `const schema = ${JSON.stringify({ identity, signature, functions: entry.functions, types: graph.types, javascriptNames: graph.javascriptNames, imports: (graph.imports ?? []).map(({ binding, contract }) => ({ binding, ...(contract ? { contract } : {}) })) })} satisfies WitClientSchema;`,
        ``,
      ]
    : [];
  return [
    `// Generated by \`client\` from @syntropika/tembra-sdk/tooling: the typed client of`,
    `// ${identity} (${source}). Regenerate it instead of editing it.`,
    ...(resources ? [`import { witClient } from "@syntropika/tembra-sdk";`] : []),
    `import type {`,
    `  AcquisitionOptions,`,
    `  Context,`,
    `  DynamicReference,`,
    `  ReferenceBase,`,
    ...(resources ? [`  WitResource,`, `  WitClientSchema,`] : []),
    `} from "@syntropika/tembra-sdk";`,
    ``,
    `/** The interface identity; an Orb's service is named \`<orb>::<identity>\`. */`,
    `export const IDENTITY = ${JSON.stringify(identity)};`,
    ``,
    ...declarations(),
    ...wrappers,
    ...resourceTypes.aliases,
    ...schema,
    `/** One object's \`${identity}\`, with the values a host sends and receives. */`,
    `export interface ${service} {`,
    `  /** The acquired reference: its readiness, status, state and \`release()\`. */`,
    `  readonly reference: ReferenceBase;`,
    ...signatures,
    `}`,
    ``,
    `/** Acquire the service of \`orb\` under \`key\` through a \`Host\` or a \`Context\`. */`,
    `export async function get(`,
    `  scope: {`,
    `    get(`,
    `      service: string,`,
    `      key: string | readonly string[],`,
    `      options?: AcquisitionOptions,`,
    `    ): Promise<DynamicReference>;`,
    `  },`,
    `  address: { orb: string; key: string | readonly string[] },`,
    `  options?: AcquisitionOptions,`,
    `): Promise<${service}> {`,
    `  const reference = await scope.get(\`\${address.orb}::\${IDENTITY}\`, address.key, options);`,
    ...(resources ? [`  const calls = witClient(reference, schema);`] : []),
    // Typed by its declaration, not by the async return, which also admits a `PromiseLike`: its
    // `Object` members would leave the arguments of `toString` or `valueOf` untyped.
    `  const client: ${service} = {`,
    `    reference,`,
    ...members,
    `  };`,
    `  return client;`,
    `}`,
    ``,
    `/** The arguments a call sends: a trailing context given as \`undefined\` is not one of them. */`,
    `function sent(args: readonly unknown[], count: number): readonly unknown[] {`,
    `  return args.length === count + 1 && args[count] === undefined ? args.slice(0, count) : args;`,
    `}`,
    ``,
  ].join("\n");
}

/**
 * Render WIT value types as the TypeScript of the host's value form; memoized per type id. A type
 * the contract names renders as its name, which takes a `_` where `taken` has it;
 * `declarations()` then answers the exported type of each one `roots` reach, in name order. The
 * names are chosen over every type `roots` reach before any renders, so that none depends on the
 * order the functions render in: two different types of one name are named after the interface
 * that declares each (`APoint`, `BPoint`), and two that still share a name are refused, each
 * named with the interface where its name is written.
 */
function renderer(
  graph: Described,
  names: TypeNames,
  identity: string,
  taken: ReadonlySet<string>,
  roots: readonly WitType[],
  resources: ReadonlyMap<number, string> = new Map(),
): { render: (type: WitType) => string; declarations: () => string[] } {
  const kinds = new Map(graph.types.map(({ id, definition }) => [id, definition.kind]));
  const wit = new Map(names.map((entry) => [entry.id, entry]));
  const rendered = new Map<number, string>();
  /** The definition an alias stands for, which a type and its aliases share. */
  const target = (type: number): number => {
    const kind = kinds.get(type);
    return typeof kind === "object" && typeof kind.type === "number" ? target(kind.type) : type;
  };
  /** Every named type the roots reach, through the definitions of the types they reach. */
  const reached = new Set<number>();
  const visited = new Set<number>();
  const reach = (type: WitType): void => {
    if (typeof type !== "number" || visited.has(type)) return;
    visited.add(type);
    // A named alias renders as its own name and declares its definition's members: the type
    // it stands for is not reached by it.
    if (wit.has(type)) reached.add(type);
    const kind = kinds.get(wit.has(type) ? target(type) : type);
    if (kind === undefined || typeof kind === "string") return;
    [
      kind.type,
      kind.list,
      kind.option,
      ...(kind.record?.fields.map((field) => field.type) ?? []),
      ...(kind.tuple?.types ?? []),
      ...(kind.variant?.cases.map((item) => item.type) ?? []),
      kind.result?.ok,
      kind.result?.err,
    ].forEach(reach);
  };
  roots.forEach(reach);
  const ordered = [...reached].sort((a, b) => a - b);
  /** The definitions each spelling alone would name in this module. */
  const spellings = new Map<string, Set<number>>();
  for (const type of ordered) {
    const { javascript } = wit.get(type)!;
    spellings.set(javascript, (spellings.get(javascript) ?? new Set()).add(target(type)));
  }
  /** The name of each named type, and the type each name declares. */
  const naming = new Map<number, string>();
  const named = new Map<string, number>();
  for (const type of ordered) {
    const entry = wit.get(type)!;
    let spelled = spellings.get(entry.javascript)!.size > 1 ? entry.qualified : entry.javascript;
    while (taken.has(spelled)) spelled += "_";
    const other = named.get(spelled);
    if (other === undefined) named.set(spelled, type);
    else if (target(other) !== target(type)) {
      const [first, second] = [wit.get(other)!, entry]
        .map((each) => `${each.name}${each.interface ? ` of ${each.interface}` : ""}`)
        .sort();
      throw new Error(`${identity}: the WIT types ${first} and ${second} both name ${spelled}`);
    }
    naming.set(type, spelled);
  }
  const refuse = (what: string): never => {
    throw new Error(`${identity}: ${what} is outside the portable value profile`);
  };
  const spelling = (name: string): string => {
    const spelled = graph.javascriptNames[name];
    if (spelled === undefined) throw new Error(`${identity}: missing JavaScript name for ${name}`);
    return spelled;
  };
  /** Whether a type renders as a union, which a list item must parenthesize. */
  const union = (type: WitType): boolean => {
    if (typeof type !== "number") return false;
    const kind = kinds.get(type);
    if (kind === undefined || typeof kind === "string" || wit.has(type)) return false;
    if (kind.type !== undefined) return union(kind.type);
    return (
      kind.option !== undefined ||
      kind.variant !== undefined ||
      kind.result !== undefined ||
      (kind.enum !== undefined && kind.enum.cases.length > 1)
    );
  };
  const payload = (tag: string, type: WitType): string =>
    type === null || type === undefined
      ? `{ tag: ${JSON.stringify(tag)} }`
      : `{ tag: ${JSON.stringify(tag)}; val: ${render(type)} }`;
  const kind = (definition: WitKind | string): string => {
    if (typeof definition === "string") return refuse(definition);
    if (definition.type !== undefined) return render(definition.type);
    if (definition.handle?.own !== undefined)
      return resources.get(target(definition.handle.own)) ?? refuse("a foreign owned resource");
    if (definition.handle?.borrow !== undefined) return refuse("an explicit resource borrow");
    if (definition.list !== undefined) {
      const item = render(definition.list);
      return union(definition.list) ? `(${item})[]` : `${item}[]`;
    }
    if (definition.option !== undefined) return `${render(definition.option)} | null`;
    if (definition.record)
      return `{ ${definition.record.fields
        .map((field) => `${spelling(field.name)}: ${render(field.type)}`)
        .join("; ")} }`;
    if (definition.tuple) return `[${definition.tuple.types.map(render).join(", ")}]`;
    if (definition.variant)
      return definition.variant.cases.map((item) => payload(item.name, item.type)).join(" | ");
    if (definition.enum)
      return definition.enum.cases.map((item) => JSON.stringify(item.name)).join(" | ");
    if (definition.flags)
      return `{ ${definition.flags.flags
        .map((flag) => `${spelling(flag.name)}?: boolean`)
        .join("; ")} }`;
    if (definition.result)
      return `${payload("ok", definition.result.ok)} | ${payload("err", definition.result.err)}`;
    return refuse(Object.keys(definition)[0] ?? "an unknown WIT type");
  };
  const render = (type: WitType): string => {
    if (type === null || type === undefined) throw new Error(`${identity}: a type is missing`);
    if (typeof type === "string") return PRIMITIVES[type] ?? refuse(type);
    const resource = resources.get(target(type));
    if (resource !== undefined) return resource;
    const known = rendered.get(type);
    if (known !== undefined) return known;
    const definition = kinds.get(type);
    if (definition === undefined) throw new Error(`${identity}: unknown WIT type ${type}`);
    const spelled = naming.get(type);
    if (spelled !== undefined) return spelled;
    const text = kind(definition);
    rendered.set(type, text);
    return text;
  };
  /** One exported type per name, for each named type the roots reach. */
  const declarations = (): string[] => {
    const declared = new Map<string, string[]>();
    for (const [spelled, type] of named) {
      const definition = kinds.get(target(type)) as WitKind;
      const what = Object.keys(definition).find((key) => definition[key] !== undefined);
      declared.set(spelled, [
        `/** The WIT ${what} \`${wit.get(type)!.name}\`. */`,
        `export type ${spelled} = ${kind(definition)};`,
        ``,
      ]);
    }
    return [...declared]
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
      .flatMap(([, lines]) => lines);
  };
  return { render, declarations };
}
