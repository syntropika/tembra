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
 * wrapper adds no behaviour: every value still crosses the same checked boundary.
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
  const source = `${path.basename(wit)}${options.world ? `, world ${options.world}` : ""}`;
  const out = path.resolve(options.out);
  const modules = new Map<string, string>();
  for (const entry of graph.exports) {
    const identity = entry.contract;
    if (!identity || !entry.functions || entry.type_only || identity === HTTP_HANDLER) continue;
    const file = path.join(out, `${identity.replace(/@[^@]*$/, "").replace(/[:/]/g, "-")}.ts`);
    if (modules.has(file))
      throw new Error(`${identity} and another exported interface share the module ${file}`);
    modules.set(file, clientModule(graph, identity, entry, source));
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
  identity: string,
  entry: Described["exports"][number],
  source: string,
): string {
  if (entry.resources && Object.keys(entry.resources).length)
    throw new Error(`${identity} declares WIT resources, which a host cannot call by name`);
  const interfaceName = /\/([^@/]+)(?:@[^@]*)?$/.exec(identity)?.[1];
  if (!interfaceName) throw new Error(`${identity} is not a versioned interface identity`);
  const spelled = graph.javascriptNames[interfaceName] ?? camel(interfaceName);
  const service = spelled.charAt(0).toUpperCase() + spelled.slice(1);
  const render = renderer(graph, identity);
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
    signatures.push(`  ${method}(${parameters.join(", ")}): Promise<${result}>;`);
    members.push(`    ${method}: (...args) => reference[${JSON.stringify(fn.name)}](...args),`);
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
    `  return {`,
    `    reference,`,
    ...members,
    `  };`,
    `}`,
    ``,
  ].join("\n");
}

/** Render WIT value types as the TypeScript of the host's value form; memoized per type id. */
function renderer(graph: Described, identity: string): (type: WitType) => string {
  const kinds = new Map(graph.types.map(({ id, definition }) => [id, definition.kind]));
  const rendered = new Map<number, string>();
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
    if (kind === undefined || typeof kind === "string") return false;
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
    const text = kind(definition);
    rendered.set(type, text);
    return text;
  };
  return render;
}
