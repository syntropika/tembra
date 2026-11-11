import { createHash } from "node:crypto";
import path from "node:path";
import ts from "typescript";
import type { NodeContract } from "@syntropika/tembra-engine-node";

export interface WitTypes {
  readonly files: ReadonlyMap<string, string>;
  readonly sha256: string;
  readonly contract: NodeContract;
  /** jco declarations of imported standard interfaces, keyed by WIT import name. */
  readonly paths: Readonly<Record<string, string[]>>;
}

/** One side entry of the contract's `describe` inspection output. */
export interface DescribedEntry {
  binding: string;
  contract?: string | null;
  type_only?: boolean;
  /** Interface functions by WIT name. */
  functions?: Readonly<Record<string, DescribedFunction>>;
  /** A freestanding world function. */
  function?: DescribedFunction;
}

export interface DescribedFunction {
  name: string;
  kind: string | Readonly<Record<string, string>>;
  params: readonly { name: string; type: unknown }[];
  result?: unknown;
}

/** The contract's inspection output: both world sides and every referenced type definition. */
export interface Description {
  imports: readonly DescribedEntry[];
  exports: readonly DescribedEntry[];
  types: readonly { id: number; definition: { kind: unknown } }[];
  javascriptNames: Readonly<Record<string, string>>;
}

/** Inspect a prepared contract without loading any guest code. */
export function describe(contract: Pick<NodeContract, "request">): Description {
  const answer = JSON.parse(contract.request(JSON.stringify({ kind: "describe" })));
  if (Object.hasOwn(answer, "ok")) return answer.ok as Description;
  const error = answer.error as { message?: unknown } | string | undefined;
  throw new Error(
    String((typeof error === "object" && error?.message) || error || "Contract inspection failed"),
  );
}

/** jco's spelling of a stream of numbers; the component runtime lifts `stream<u8>` as chunks. */
const BYTE_STREAM = "AsyncIterable<number>";
/** jco's spelling of a future; the component runtime's future handle is read, not awaited. */
const FUTURE = "PromiseLike<";
const NUMERIC = new Set(["u8", "u16", "u32", "u64", "s8", "s16", "s32", "s64", "f32", "f64"]);

/** Whether every numeric stream element of the contract is `u8`. */
function byteStreams(description: Description): boolean {
  const kinds = new Map(description.types.map((type) => [type.id, type.definition.kind]));
  const resolve = (type: unknown): unknown => {
    for (let depth = 0; typeof type === "number" && depth < 64; depth++) {
      const kind = kinds.get(type) as { type?: unknown } | undefined;
      if (!kind || typeof kind !== "object" || !("type" in kind)) break;
      type = kind.type;
    }
    return type;
  };
  for (const kind of kinds.values()) {
    if (!kind || typeof kind !== "object" || !("stream" in kind)) continue;
    const element = resolve((kind as { stream: unknown }).stream);
    if (typeof element === "string" && NUMERIC.has(element) && element !== "u8") return false;
  }
  return true;
}

/**
 * Rewrite jco's `PromiseLike<T>` futures. Futures appear only in imported (standard) functions:
 * an argument may be a Promise, which the runtime lowers into a future, or a future handle; a
 * returned future is a handle read with `read()`.
 */
function futures(name: string, text: string): string {
  const source = ts.createSourceFile(name, text, ts.ScriptTarget.ESNext, true);
  const edits: { start: number; end: number; text: string }[] = [];
  function visit(node: ts.Node, argument: boolean): void {
    if (
      ts.isTypeReferenceNode(node) &&
      ts.isIdentifier(node.typeName) &&
      node.typeName.text === "PromiseLike" &&
      node.typeArguments?.length === 1
    ) {
      const inner = node.typeArguments[0]!;
      // Nested futures are rewritten inside the type argument first.
      const value = rewrite(inner, argument);
      edits.push({
        start: node.getStart(source),
        end: node.getEnd(),
        text: argument
          ? `(PromiseLike<${value}> | TembraWitFuture<${value}>)`
          : `TembraWitFuture<${value}>`,
      });
      return;
    }
    ts.forEachChild(node, (child) => visit(child, argument || ts.isParameter(child)));
  }
  function rewrite(node: ts.Node, argument: boolean): string {
    const start = node.getStart(source);
    const nested: typeof edits = [];
    const saved = edits.splice(0);
    visit(node, argument);
    nested.push(...edits.splice(0));
    edits.push(...saved);
    let value = text.slice(start, node.getEnd());
    for (const edit of nested.sort((left, right) => right.start - left.start))
      value = value.slice(0, edit.start - start) + edit.text + value.slice(edit.end - start);
    return value;
  }
  visit(source, false);
  let result = text;
  for (const edit of edits.sort((left, right) => right.start - left.start))
    result = result.slice(0, edit.start) + edit.text + result.slice(edit.end);
  return result;
}

/**
 * Correct jco declarations to the component runtime's values: `stream<u8>` is an async iterable
 * of `Uint8Array` chunks, a future is a handle read with `read()`, wide numeric lists may be
 * arrays, and resources are disposable.
 */
function declarations(name: string, text: string): string {
  const source = ts.createSourceFile(name, text, ts.ScriptTarget.ESNext, true);
  // WIT resources always have a canonical drop. The pinned jco declarations omit it;
  // declaration merging exposes the runtime's Symbol.dispose without copying WIT types.
  const resources = source.statements.filter(
    (statement): statement is ts.ClassDeclaration =>
      ts.isClassDeclaration(statement) &&
      statement.modifiers?.some((modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword) ===
        true,
  );
  const additions = resources.map(
    (resource) => `export interface ${resource.name!.text} extends Disposable {}`,
  );
  // The runtime lifts only `list<u8>`/`list<s8>` as typed arrays; other numeric lists arrive as
  // arrays, and either form is accepted when lowering.
  let corrected = text
    .replaceAll(BYTE_STREAM, "AsyncIterable<Uint8Array>")
    .replace(
      /\b(Int16Array|Uint16Array|Int32Array|Uint32Array|Float32Array|Float64Array)\b/g,
      "($1 | number[])",
    )
    .replace(/\b(BigInt64Array|BigUint64Array)\b/g, "($1 | bigint[])");
  if (corrected.includes(FUTURE)) {
    corrected = futures(name, corrected);
    additions.push(
      "/** A lifted WIT future handle: not thenable, `read()` returns its value once. */",
      "interface TembraWitFuture<T> { read(): Promise<T>; drop(): void; }",
    );
  }
  if (!additions.length) return corrected;
  return (
    (resources.length ? '/// <reference lib="esnext.disposable" />\n' : "") +
    corrected +
    "\n" +
    additions.join("\n") +
    "\n"
  );
}

/** Generate official WIT declarations in memory and expose a type-only .wit module. */
export async function witTypes(file: string, world?: string): Promise<WitTypes> {
  const { NodeContract } = await import("@syntropika/tembra-engine-node");
  const { $init, generateTypes } = await import("@bytecodealliance/jco-transpile/component");
  const absolute = path.resolve(file);
  if (!absolute.endsWith(".wit")) throw new Error("Direct WIT imports require a .wit entry file");
  const contract = new NodeContract(path.dirname(absolute), world);
  await $init;
  const generated = generateTypes("contract", {
    wit: { tag: "binary", val: contract.wit() },
    ...(world ? { world } : {}),
  });
  const description = describe(contract);
  const chunked = byteStreams(description);
  const entries = [...generated]
    .map(([name, bytes]): [string, Uint8Array] => {
      if (!name.endsWith(".d.ts")) return [name, bytes];
      const text = new TextDecoder().decode(bytes);
      if (!chunked && text.includes(BYTE_STREAM))
        throw new Error(`${name}: only stream<u8> is supported among numeric stream elements`);
      return [name, new TextEncoder().encode(declarations(name, text))];
    })
    .sort(([left], [right]) => left.localeCompare(right));
  const digest = createHash("sha256");
  for (const [name, bytes] of entries)
    digest.update(JSON.stringify([name, bytes.length])).update(bytes);
  const sha256 = digest.digest("hex");
  const directory = path.join(path.dirname(absolute), ".tembra", "types", sha256);
  const files = new Map(
    entries.map(([name, bytes]) => [path.join(directory, name), new TextDecoder().decode(bytes)]),
  );
  const alias = absolute.slice(0, -4) + ".d.wit.ts";
  const target =
    "./" +
    path
      .relative(path.dirname(alias), path.join(directory, "contract.js"))
      .split(path.sep)
      .join("/");
  files.set(alias, `export * from ${JSON.stringify(target)};\n`);
  // Standard imports retain jco's WIT shapes and the canonical resource-drop declaration.
  const paths: Record<string, string[]> = {};
  for (const imported of description.imports) {
    if (!imported.contract?.startsWith("wasi:")) continue;
    const [, namespace, name] = /^(\w+):(.+)@/.exec(imported.contract)!;
    const declaration = path.join(
      directory,
      "interfaces",
      `${namespace}-${name!.replace("/", "-")}.d.ts`,
    );
    if (files.has(declaration)) paths[imported.contract] = [declaration];
  }
  return { files, sha256, contract, paths };
}
