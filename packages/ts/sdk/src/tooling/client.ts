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
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import path from "node:path";
import { NodeContract } from "@syntropika/tembra-engine-node";
import type { ReferenceBase } from "../authoring/types.ts";
import { HTTP_HANDLER } from "../interpreter/http-ingress.ts";
import { query } from "../interpreter/types.ts";

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
  [other: string]: unknown;
}
interface DescribedFunction {
  readonly name: string;
  readonly params: readonly { readonly name: string; readonly type: WitType }[];
  readonly result?: WitType;
}
/** The parts of the contract's `describe` answer this module reads. */
interface Described {
  readonly exports: readonly {
    readonly contract?: string | null;
    readonly type_only?: boolean;
    readonly resources?: Readonly<Record<string, number>>;
    readonly functions?: Readonly<Record<string, DescribedFunction>>;
  }[];
  readonly types: readonly {
    readonly id: number;
    readonly definition: { readonly kind: WitKind | string };
  }[];
  readonly javascriptNames: Readonly<Record<string, string>>;
}
/** The contract's `type-names` answer: a WIT name for some ids of the describe graph. */
type TypeNames = readonly {
  readonly id: number;
  readonly name: string;
  readonly javascript: string;
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
]);

/** The default JavaScript spelling of a WIT name, as the contract derives it for members. */
function camel(name: string): string {
  return name
    .replace(/^%/, "")
    .split("-")
    .map((word, index) => (index ? word.charAt(0).toUpperCase() + word.slice(1) : word))
    .join("");
}

/** Write one typed client module per exported interface; returns every module's path. */
export async function client(options: ClientOptions): Promise<readonly string[]> {
  const wit = path.resolve(options.wit);
  if (!wit.endsWith(".wit")) throw new Error("Client generation requires a .wit entry file");
  const contract = new NodeContract(path.dirname(wit), options.world);
  const graph = query<Described>(contract, { kind: "describe" });
  const names = query<TypeNames>(contract, { kind: "type-names" });
  const source = `${path.basename(wit)}${options.world ? `, world ${options.world}` : ""}`;
  const out = path.resolve(options.out);
  const modules = new Map<string, string>();
  for (const entry of graph.exports) {
    const identity = entry.contract;
    if (!identity || !entry.functions || entry.type_only || identity === HTTP_HANDLER) continue;
    const file = path.join(out, `${identity.replace(/@[^@]*$/, "").replace(/[:/]/g, "-")}.ts`);
    if (modules.has(file))
      throw new Error(`${identity} and another exported interface share the module ${file}`);
    modules.set(file, clientModule(graph, names, identity, entry, source));
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
): string {
  if (entry.resources && Object.keys(entry.resources).length)
    throw new Error(`${identity} declares WIT resources, which a host cannot call by name`);
  const interfaceName = /\/([^@/]+)(?:@[^@]*)?$/.exec(identity)?.[1];
  if (!interfaceName) throw new Error(`${identity} is not a versioned interface identity`);
  const spelled = graph.javascriptNames[interfaceName] ?? camel(interfaceName);
  const pascal = spelled.charAt(0).toUpperCase() + spelled.slice(1);
  const service = OWN.has(pascal) ? `${pascal}_` : pascal;
  const { render, declarations } = renderer(graph, names, identity, new Set([...OWN, service]));
  const signatures: string[] = [];
  const members: string[] = [];
  for (const fn of Object.values(entry.functions ?? {}).sort((a, b) =>
    a.name.localeCompare(b.name),
  )) {
    const method = graph.javascriptNames[fn.name] ?? camel(fn.name);
    if (Object.hasOwn(MEMBERS, fn.name) || Object.hasOwn(MEMBERS, method))
      throw new Error(
        `${identity}: ${fn.name} collides with a member every client has ` +
          `(${Object.keys(MEMBERS).join(", ")})`,
      );
    const taken = new Set<string>();
    const parameters = fn.params.map((parameter) => {
      let name = camel(parameter.name);
      while (RESERVED.has(name) || taken.has(name)) name += "_";
      taken.add(name);
      return `${name}: ${render(parameter.type)}`;
    });
    parameters.push("context?: Context");
    const result = fn.result === undefined || fn.result === null ? "void" : render(fn.result);
    // In an interface, `new(..)` declares a construct signature; the quoted name is a method.
    const key = method === "new" ? JSON.stringify(method) : method;
    signatures.push(`  ${key}(${parameters.join(", ")}): Promise<${result}>;`);
    members.push(
      `    ${method}: (...args) => reference[${JSON.stringify(fn.name)}](...sent(args, ${fn.params.length})),`,
    );
  }
  return [
    `// Generated by \`client\` from @syntropika/tembra-sdk/tooling: the typed client of`,
    `// ${identity} (${source}). Regenerate it instead of editing it.`,
    `import type {`,
    `  AcquisitionOptions,`,
    `  Context,`,
    `  DynamicReference,`,
    `  ReferenceBase,`,
    `} from "@syntropika/tembra-sdk";`,
    ``,
    `/** The interface identity; an Orb's service is named \`<orb>::<identity>\`. */`,
    `export const IDENTITY = ${JSON.stringify(identity)};`,
    ``,
    ...declarations(),
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
 * the contract names renders as its name, which takes a `_` where `taken` or an earlier type of
 * the module has it; `declarations()` then answers the exported type of each name rendered.
 */
function renderer(
  graph: Described,
  names: TypeNames,
  identity: string,
  taken: Set<string>,
): { render: (type: WitType) => string; declarations: () => string[] } {
  const kinds = new Map(graph.types.map(({ id, definition }) => [id, definition.kind]));
  const wit = new Map(names.map((entry) => [entry.id, entry]));
  const rendered = new Map<number, string>();
  /** The named types rendered so far, in the order they were first rendered. */
  const named = new Map<number, string>();
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
    const known = rendered.get(type);
    if (known !== undefined) return known;
    const definition = kinds.get(type);
    if (definition === undefined) throw new Error(`${identity}: unknown WIT type ${type}`);
    const name = wit.get(type);
    if (name !== undefined) {
      let spelled = name.javascript;
      while (taken.has(spelled)) spelled += "_";
      taken.add(spelled);
      named.set(type, spelled);
      rendered.set(type, spelled);
      return spelled;
    }
    const text = kind(definition);
    rendered.set(type, text);
    return text;
  };
  /** One exported type per named type rendered, those its definition renders included. */
  const declarations = (): string[] => {
    const lines: string[] = [];
    for (const [type, spelled] of named) {
      const definition = kinds.get(type) as WitKind;
      const what = Object.keys(definition).find((key) => definition[key] !== undefined);
      lines.push(
        `/** The WIT ${what} \`${wit.get(type)!.name}\`. */`,
        `export type ${spelled} = ${kind(definition)};`,
        ``,
      );
    }
    return lines;
  };
  return { render, declarations };
}
